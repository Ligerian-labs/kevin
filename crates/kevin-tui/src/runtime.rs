//! The impure half: a terminal, a [`KevinClient`] and the task that turns
//! [`Cmd`]s into [`Msg`]s (`plan/07-api-and-tui.md` §4).
//!
//! Everything here is plumbing. The reducer decides *what* to do; this module
//! only performs it and feeds the answer back, which is why the screens can be
//! tested without a terminal and the state machine without a server.

use std::io::Write as _;
use std::time::Duration;

use futures::StreamExt as _;
use kevin_api::client::{ClientError, KevinClient};
use kevin_api::dto::{
    CostQueryDto, CreateRunRequest, LessonsQuery, ListRunsQuery, MemorySearchQuery, ProposalsQuery,
    QuestionsQuery, TaskLogQueryDto,
};
use kevin_domain::ids::RunId;
use secrecy::SecretString;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use url::Url;

use crate::keys::KeyPress;
use crate::model::Model;
use crate::msg::{Cmd, Msg};
use crate::theme::Theme;
use crate::update::{init, update};
use crate::view::view;

/// How often the TUI re-polls the snapshot endpoints of the visible screen.
pub const DEFAULT_POLL: Duration = Duration::from_millis(1_000);
/// How long the key reader waits for a key before yielding to the runtime.
const KEY_POLL: Duration = Duration::from_millis(100);
/// Log lines fetched per poll.
const LOG_PAGE: usize = 200;

/// How to start a session.
#[derive(Debug, Clone)]
pub struct Options {
    /// The daemon to talk to.
    pub server: Url,
    /// Bearer token for that daemon.
    pub token: SecretString,
    /// Open this run straight away (`kevin tui --run`).
    pub run: Option<RunId>,
    /// Snapshot poll interval.
    pub poll: Duration,
}

impl Options {
    /// Options with the default poll interval.
    #[must_use]
    pub fn new(server: Url, token: SecretString) -> Self {
        Self {
            server,
            token,
            run: None,
            poll: DEFAULT_POLL,
        }
    }

    /// Parses `server` and wraps `token`, so `kevin-cli` needs neither `url`
    /// nor `secrecy` to open a session.
    pub fn connect(server: &str, token: &str) -> Result<Self, Error> {
        let server = Url::parse(server)
            .map_err(|e| Error::Client(ClientError::Url(format!("{server}: {e}"))))?;
        Ok(Self::new(server, SecretString::from(token.trim())))
    }

    /// Builder: open `run` on start.
    #[must_use]
    pub fn run(mut self, run: Option<RunId>) -> Self {
        self.run = run;
        self
    }

    /// Builder: how often the visible screen is re-polled.
    #[must_use]
    pub const fn poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }
}

/// Anything that can stop a session.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The terminal could not be set up or restored.
    #[error("terminal: {0}")]
    Terminal(#[from] std::io::Error),
    /// The first call to the daemon failed, so there is nothing to show.
    #[error("{0}")]
    Client(#[from] ClientError),
}

/// Opens the terminal and runs the session until the operator quits.
pub async fn run(options: Options) -> Result<(), Error> {
    let client = KevinClient::new(options.server.clone(), options.token.clone());
    // Fail before taking over the terminal when the daemon is unreachable.
    client.drain_status().await?;

    let mut terminal = ratatui::try_init()?;
    let result = event_loop(&mut terminal, &client, &options).await;
    ratatui::try_restore()?;
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    client: &KevinClient,
    options: &Options,
) -> Result<(), Error> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut model = Model::new(options.server.as_str());
    model.theme = Theme::from_env();
    if let Ok(size) = terminal.size() {
        model.size = (size.width, size.height);
    }
    model.now = chrono::Utc::now();

    let keys = spawn_keys(tx.clone());
    let ticks = spawn_ticks(tx.clone(), options.poll);
    let mut streams = Streams::default();

    let mut pending = init(options.run);
    if options.run.is_some() {
        model.screen = crate::model::Screen::RunDetail;
    }
    loop {
        for cmd in pending.drain(..) {
            perform(cmd, client, &tx, &mut streams);
        }
        terminal.draw(|frame| view(&model, frame))?;
        if model.quit {
            break;
        }
        let Some(msg) = rx.recv().await else { break };
        pending = update(&mut model, msg);
        // Drain whatever else arrived so a burst of events costs one redraw.
        while let Ok(msg) = rx.try_recv() {
            pending.extend(update(&mut model, msg));
        }
    }

    keys.abort();
    ticks.abort();
    streams.abort();
    Ok(())
}

/// The two long-lived subscriptions, so they can be replaced on resync.
#[derive(Debug, Default)]
struct Streams {
    events: Option<JoinHandle<()>>,
    log: Option<JoinHandle<()>>,
}

impl Streams {
    fn abort(&mut self) {
        if let Some(handle) = self.events.take() {
            handle.abort();
        }
        if let Some(handle) = self.log.take() {
            handle.abort();
        }
    }
}

fn spawn_keys(tx: mpsc::UnboundedSender<Msg>) -> JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        loop {
            match crossterm::event::poll(KEY_POLL) {
                Ok(false) => {
                    if tx.is_closed() {
                        return;
                    }
                    continue;
                }
                Ok(true) => {}
                Err(err) => {
                    let _ = tx.send(Msg::ClientError(format!("terminal input: {err}")));
                    return;
                }
            }
            let sent = match crossterm::event::read() {
                Ok(crossterm::event::Event::Key(key)) => KeyPress::from_crossterm(&key)
                    .map(Msg::Key)
                    .is_none_or(|msg| tx.send(msg).is_ok()),
                Ok(crossterm::event::Event::Resize(cols, rows)) => {
                    tx.send(Msg::Resized(cols, rows)).is_ok()
                }
                Ok(_) => true,
                Err(err) => {
                    let _ = tx.send(Msg::ClientError(format!("terminal input: {err}")));
                    return;
                }
            };
            if !sent {
                return;
            }
        }
    })
}

fn spawn_ticks(tx: mpsc::UnboundedSender<Msg>, every: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(every);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if tx.send(Msg::Tick(chrono::Utc::now())).is_err() {
                return;
            }
        }
    })
}

/// Runs one [`Cmd`]: streams and terminal effects are handled here, every
/// HTTP command goes through [`execute`] on a spawned task.
fn perform(cmd: Cmd, client: &KevinClient, tx: &mpsc::UnboundedSender<Msg>, streams: &mut Streams) {
    let client = client.clone();
    let tx = tx.clone();
    match cmd {
        Cmd::Quit => {
            let _ = tx.send(Msg::Quit);
        }
        Cmd::Yank(text) => {
            copy_to_clipboard(&text);
            let _ = tx.send(Msg::Notice(format!("yanked {text}")));
        }
        Cmd::Subscribe(from) => {
            if let Some(handle) = streams.events.take() {
                handle.abort();
            }
            tracing::debug!(from, "subscribing to the event firehose");
            streams.events = Some(tokio::spawn(async move {
                let mut stream = Box::pin(client.events(Some(EVENT_TYPES), from));
                while let Some(item) = stream.next().await {
                    if tx.send(event_msg(item)).is_err() {
                        return;
                    }
                }
            }));
        }
        Cmd::FollowTaskLog(task_id) => {
            if let Some(handle) = streams.log.take() {
                handle.abort();
            }
            tracing::debug!(%task_id, "following a task transcript");
            streams.log = Some(tokio::spawn(async move {
                let mut stream = Box::pin(client.task_log_stream(task_id, None));
                while let Some(item) = stream.next().await {
                    let msg = match item {
                        Ok(line) => Msg::LogLines(task_id, vec![line]),
                        Err(ClientError::Resync) => Msg::Resync,
                        Err(err) => Msg::StreamError(err.to_string()),
                    };
                    if tx.send(msg).is_err() {
                        return;
                    }
                }
            }));
        }
        Cmd::UnfollowTaskLog => {
            if let Some(handle) = streams.log.take() {
                handle.abort();
            }
        }
        cmd => {
            tokio::spawn(async move {
                if let Some(result) = execute(&client, cmd).await {
                    let _ = tx.send(result.unwrap_or_else(|err| Msg::ClientError(err.to_string())));
                }
            });
        }
    }
}

/// The event types the TUI subscribes to on the firehose.
pub const EVENT_TYPES: &str = "run.*,task.*,question.*";

/// Maps one item of the SSE firehose onto a [`Msg`].
#[must_use]
pub fn event_msg(item: Result<kevin_api::dto::EventDto, ClientError>) -> Msg {
    match item {
        Ok(event) => Msg::ApiEvent(Box::new(event)),
        Err(ClientError::Resync) => Msg::Resync,
        Err(err) => Msg::StreamError(err.to_string()),
    }
}

/// Performs one HTTP [`Cmd`] and returns the [`Msg`] it produces.
///
/// `None` for the commands the event loop handles itself (streams, clipboard,
/// quit). Exposed so the acceptance tests can drive the reducer's output
/// against `kevin_testkit::fake_api` without a terminal.
#[allow(
    clippy::too_many_lines,
    reason = "one arm per command; splitting it would only hide the mapping"
)]
pub async fn execute(client: &KevinClient, cmd: Cmd) -> Option<Result<Msg, ClientError>> {
    let result =
        match cmd {
            Cmd::Quit
            | Cmd::Yank(_)
            | Cmd::Subscribe(_)
            | Cmd::FollowTaskLog(_)
            | Cmd::UnfollowTaskLog => return None,

            Cmd::FetchRuns => client
                .list_runs(&ListRunsQuery::default())
                .await
                .map(|page| Msg::RunsLoaded(page.items)),
            Cmd::FetchRun(id) => client
                .get_run(id)
                .await
                .map(|run| Msg::RunLoaded(Box::new(run))),
            Cmd::FetchTasks(id) => client
                .run_tasks(id)
                .await
                .map(|tasks| Msg::TasksLoaded(id, tasks)),
            Cmd::FetchTaskLog { task_id, after_seq } => {
                let query = TaskLogQueryDto {
                    attempt: None,
                    after_seq,
                    limit: Some(LOG_PAGE),
                };
                client
                    .task_log(task_id, &query)
                    .await
                    .map(|page| Msg::LogLines(task_id, page.items))
            }
            Cmd::FetchQuestions => {
                let query = QuestionsQuery {
                    status: Some("open".to_owned()),
                    ..QuestionsQuery::default()
                };
                client
                    .questions(&query)
                    .await
                    .map(|page| Msg::QuestionsLoaded(page.items))
            }
            Cmd::FetchRoutes(kind) => client.routes(kind.as_deref()).await.map(Msg::RoutesLoaded),
            Cmd::FetchLessons => client
                .lessons(&LessonsQuery::default())
                .await
                .map(|page| Msg::LessonsLoaded(page.items)),
            Cmd::FetchProposals => {
                let query = ProposalsQuery {
                    status: Some("proposed".to_owned()),
                    ..ProposalsQuery::default()
                };
                client
                    .proposals(&query)
                    .await
                    .map(|page| Msg::ProposalsLoaded(page.items))
            }
            Cmd::FetchWorkers => client.workers().await.map(Msg::WorkersLoaded),
            Cmd::FetchCost(run_id) => {
                let query = CostQueryDto {
                    run_id,
                    ..CostQueryDto::default()
                };
                client
                    .cost(&query)
                    .await
                    .map(|report| Msg::CostLoaded(Box::new(report)))
            }
            Cmd::FetchDrain => client.drain_status().await.map(Msg::DrainLoaded),
            Cmd::SearchMemory(q) => {
                let query = MemorySearchQuery {
                    q,
                    ..MemorySearchQuery::default()
                };
                client.memory_search(&query).await.map(Msg::LessonsLoaded)
            }

            Cmd::CreateRun(goal) => {
                let request = CreateRunRequest {
                    goal,
                    cwd: None,
                    attachments: Vec::new(),
                    mode: None,
                    budget: None,
                    tags: Vec::new(),
                };
                let key = format!("tui-{}", uuid::Uuid::now_v7());
                client
                    .create_run(request, Some(&key))
                    .await
                    .map(|run| Msg::RunLoaded(Box::new(run)))
            }
            Cmd::CancelRun(id, reason) => client
                .cancel_run(id, reason)
                .await
                .map(|run| Msg::RunLoaded(Box::new(run))),
            Cmd::ApprovePlan(id) => client
                .approve_plan(id, None)
                .await
                .map(|run| Msg::RunLoaded(Box::new(run))),
            Cmd::RejectPlan(id, feedback) => client
                .reject_plan(id, feedback)
                .await
                .map(|run| Msg::RunLoaded(Box::new(run))),
            Cmd::RetryTask(id, exclude) => client
                .retry_task(id, exclude)
                .await
                .map(|task| Msg::Notice(format!("retrying {}: {}", task.id, task.status))),
            Cmd::CancelTask(id) => client
                .cancel_task(id)
                .await
                .map(|task| Msg::Notice(format!("cancelled {}", task.id))),
            Cmd::AnswerQuestion(id, answer) => client
                .answer_question(id, answer, None)
                .await
                .map(|question| Msg::QuestionAnswered(Box::new(question))),
            Cmd::AcceptProposal(id) => client.accept_proposal(id, None).await.map(|proposal| {
                Msg::Notice(format!("proposal {} {}", proposal.id, proposal.status))
            }),
            Cmd::RejectProposal(id) => client.reject_proposal(id, None).await.map(|proposal| {
                Msg::Notice(format!("proposal {} {}", proposal.id, proposal.status))
            }),
            Cmd::ForgetLesson(id) => client
                .forget_memory(id)
                .await
                .map(|()| Msg::Notice(format!("forgot {id}"))),
        };
    Some(result)
}

/// OSC 52: asks the terminal to put `text` on the system clipboard. Terminals
/// that do not support it ignore the sequence, which is why `y` also reports
/// the value in the status bar.
fn copy_to_clipboard(text: &str) {
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = stdout.flush();
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        for i in 0..4 {
            if i <= chunk.len() {
                let index = (triple >> (18 - 6 * i)) & 0x3f;
                out.push(char::from(ALPHABET[index as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_matches_the_rfc_examples() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
