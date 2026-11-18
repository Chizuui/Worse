use std::cell::RefCell;

use harness_core_api::{
    ApiError, GhStatus, InboxSource, InboxSourceInput, InboxStatus, Ticket, TicketStatus,
    TriggerRecord,
};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use crate::core::{ChatCall, Command, Reply};
use crate::ui::{
    AppWindow, InboxNotice, InboxSourceRow, InboxTicketRow, InboxTriggerRow, InboxUi, SelectOption,
    SettingsUi,
};

#[derive(Default)]
struct State {
    window: Option<Weak<AppWindow>>,
    status: InboxStatus,
    tickets: Vec<Ticket>,
    triggers: Vec<TriggerRecord>,
    /// Notices updated before this instant count as read (`Mark all read`).
    read_at: i64,
    /// Inbox list filter: all | unread | subagents | builds.
    filter: String,
    pending: Option<slint::Timer>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn sv(text: &str) -> SharedString {
    SharedString::from(text)
}

fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::from(std::rc::Rc::new(VecModel::from(rows)))
}

fn window() -> Option<AppWindow> {
    STATE.with(|s| s.borrow().window.as_ref().and_then(Weak::upgrade))
}

fn with_ui(f: impl FnOnce(InboxUi<'_>)) {
    if let Some(w) = window() {
        f(w.global::<InboxUi>());
    }
}

fn set_error(text: &str) {
    with_ui(|ui| ui.set_error(sv(text)));
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn ago(ms: i64) -> String {
    let secs = (now_ms() - ms).max(0) / 1000;
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{} min ago", secs / 60)
    } else if secs < 86_400 {
        format!("{} h ago", secs / 3600)
    } else {
        format!("{} d ago", secs / 86_400)
    }
}

fn call<T: 'static>(make: impl FnOnce(Reply<T>) -> ChatCall, done: impl FnOnce(T) + 'static) {
    let (reply, answer) = tokio::sync::oneshot::channel();
    crate::bridge::send(Command::Chat(make(reply)));
    let _ = slint::spawn_local(async move {
        match answer.await.unwrap_or(Err(ApiError::ShutDown)) {
            Ok(value) => done(value),
            Err(error) => set_error(&error.to_string()),
        }
    });
}

fn agent_options() -> Vec<(String, String)> {
    let mut out = vec![(String::new(), "Default agent".to_owned())];
    out.extend(
        crate::bridge::orchestra_agents()
            .into_iter()
            .filter(|a| a.2)
            .map(|a| (a.0, a.1)),
    );
    out
}

fn agent_label(id: &str) -> String {
    agent_options()
        .into_iter()
        .find(|a| a.0 == id)
        .map_or_else(|| id.to_owned(), |a| a.1)
}

fn gh_text(status: &InboxStatus) -> String {
    match status.gh {
        GhStatus::Missing => {
            "GitHub CLI (gh) was not found. Harness can download the official release for you."
                .into()
        }
        GhStatus::SignedOut => format!(
            "gh is installed but not signed in. Sign in yourself by running this in a terminal: {}",
            gh_command(status)
        ),
        GhStatus::Ready if status.gh_detail.is_empty() => "gh is ready.".into(),
        GhStatus::Ready => format!("Signed in to GitHub as {}.", status.gh_detail),
    }
}

fn gh_command(status: &InboxStatus) -> String {
    let path = if status.gh_path.is_empty() {
        "gh".to_owned()
    } else {
        format!("\"{}\"", status.gh_path)
    };
    format!("{path} auth login --web -s project")
}

pub fn gh_install_event(status: harness_core_api::AdapterInstallStatus, message: Option<String>) {
    if status == harness_core_api::AdapterInstallStatus::Running {
        with_ui(|ui| ui.set_gh_installing(true));
        return;
    }
    with_ui(|ui| ui.set_gh_installing(false));
    if status == harness_core_api::AdapterInstallStatus::Failed {
        set_error(&message.unwrap_or_default());
    }
    refresh();
}

fn source_row(source: &InboxSource) -> InboxSourceRow {
    let users = if source.allowlist.is_empty() {
        "nobody allowed yet".to_owned()
    } else {
        source.allowlist.join(", ")
    };
    let every = format!("every {} min", source.interval_secs / 60);
    let (detail, failing) = if !source.enabled {
        (format!("Paused · {} from {users}", source.handle), false)
    } else if let Some(error) = &source.last_error {
        (format!("Last check failed: {error}"), true)
    } else {
        let checked = source.last_poll.map_or_else(
            || "not checked yet".to_owned(),
            |t| format!("checked {}", ago(t)),
        );
        (
            format!("{} from {users} · {every} · {checked}", source.handle),
            false,
        )
    };
    let mut detail = detail;
    if source.route {
        detail.push_str(" · routed by Orchestra");
    }
    if !source.shadow_labels.is_empty() {
        detail.push_str(&format!(" · shadow: {}", source.shadow_labels.join(", ")));
    }
    InboxSourceRow {
        id: sv(&source.id),
        repo: sv(&source.repo),
        folder: sv(&source.workspace_path),
        detail: sv(&detail),
        enabled: source.enabled,
        failing,
        has_router: source.router_session_id.is_some(),
    }
}

fn ticket_row(ticket: &Ticket) -> InboxTicketRow {
    let (status, tone) = match ticket.status {
        TicketStatus::Working => ("Working", "progress"),
        TicketStatus::NeedsApproval => ("Needs approval", "warning"),
        TicketStatus::Review if ticket.shadow => ("Proposal ready", "accent"),
        TicketStatus::Review if ticket.pending_post => ("Draft ready", "accent"),
        TicketStatus::Review => ("In review", "success"),
        TicketStatus::Done => ("Done", "success"),
        TicketStatus::Failed => ("Failed", "destructive"),
        TicketStatus::Discarded => ("Discarded", "neutral"),
    };
    let has_session = ticket.session_id.is_some();
    InboxTicketRow {
        id: sv(&ticket.id),
        title: sv(&format!("#{} {}", ticket.number, ticket.title)),
        meta: sv(&format!(
            "{} · from {} · {}",
            ticket.repo,
            ticket.requester,
            ago(ticket.updated_at)
        )),
        status: sv(status),
        tone: sv(tone),
        pr: sv(ticket.pr_url.as_deref().unwrap_or_default()),
        error: sv(ticket.error.as_deref().unwrap_or_default()),
        has_session,
        can_post: has_session
            && (ticket.pending_post
                || ticket.status == TicketStatus::Failed
                || (ticket.shadow && ticket.status.is_active())),
        can_stop: matches!(
            ticket.status,
            TicketStatus::Working | TicketStatus::NeedsApproval
        ),
        can_discard: !matches!(ticket.status, TicketStatus::Done | TicketStatus::Discarded),
        shadow: ticket.shadow,
    }
}

fn trigger_row(record: &TriggerRecord) -> InboxTriggerRow {
    InboxTriggerRow {
        title: sv(&format!(
            "{} on {}#{} ({})",
            record.author, record.repo, record.number, record.kind
        )),
        verdict: sv(&record.reason),
        accepted: record.accepted,
        time: sv(&ago(record.received_at)),
    }
}

pub fn install(window: &AppWindow) {
    STATE.with(|s| s.borrow_mut().window = Some(window.as_weak()));
    let ui = window.global::<InboxUi>();
    ui.on_set_enabled(|on| {
        with_ui(|ui| ui.set_enabled(on));
        call(
            |reply| ChatCall::SetInboxEnabled(on, reply),
            |()| {
                set_error("");
                refresh();
            },
        );
    });
    ui.on_action(|action, id| action_for(&action, &id));
    ui.on_save_form(save_form);
}

pub fn open() {
    set_error("");
    refresh();
}

pub fn changed() {
    let Some(w) = window() else {
        return;
    };
    if w.get_settings_open() && w.global::<SettingsUi>().get_page() == "inbox" {
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            if s.pending.is_none() {
                let timer = slint::Timer::default();
                timer.start(
                    slint::TimerMode::SingleShot,
                    std::time::Duration::from_millis(150),
                    || {
                        STATE.with(|s| s.borrow_mut().pending = None);
                        refresh();
                    },
                );
                s.pending = Some(timer);
            }
        });
    }
}

fn refresh() {
    call(ChatCall::InboxStatus, |status: InboxStatus| {
        with_ui(|ui| {
            ui.set_enabled(status.enabled);
            ui.set_gh_ready(status.gh == GhStatus::Ready);
            ui.set_gh_text(sv(&gh_text(&status)));
            ui.set_gh_state(match status.gh {
                GhStatus::Ready => 0,
                GhStatus::Missing => 1,
                GhStatus::SignedOut => 2,
            });
            ui.set_sources(model(status.sources.iter().map(source_row).collect()));
        });
        STATE.with(|s| s.borrow_mut().status = status);
    });
    call(ChatCall::Tickets, |tickets: Vec<Ticket>| {
        with_ui(|ui| ui.set_tickets(model(tickets.iter().map(ticket_row).collect())));
        STATE.with(|s| s.borrow_mut().tickets = tickets);
        render_notices();
    });
    call(
        |reply| ChatCall::Triggers(50, reply),
        |list: Vec<TriggerRecord>| {
            with_ui(|ui| ui.set_triggers(model(list.iter().map(trigger_row).collect())));
            STATE.with(|s| s.borrow_mut().triggers = list);
            render_notices();
        },
    );
}

fn rgb(hex: u32) -> slint::Color {
    slint::Color::from_argb_encoded(0xFF00_0000 | hex)
}

fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().collect();
    match words.as_slice() {
        [] => "◎".into(),
        [one] if one.chars().count() <= 3 => (*one).to_owned(),
        [one] => one.chars().take(2).collect::<String>().to_uppercase(),
        [a, b, ..] => format!(
            "{}{}",
            a.chars().next().unwrap_or_default(),
            b.chars().next().unwrap_or_default()
        )
        .to_uppercase(),
    }
}

/// One design S4 inbox row: a ticket (work by a subagent) or a GitHub trigger (pull request / mention).
struct Notice {
    row: InboxNotice,
    at: i64,
    subagent: bool,
    build: bool,
}

fn notices(s: &State) -> Vec<Notice> {
    let mut out = Vec::new();
    for t in &s.tickets {
        let agent = s
            .status
            .sources
            .iter()
            .find(|x| x.id == t.source_id)
            .and_then(|x| x.agent.as_ref())
            .map(|a| a.as_str().to_owned())
            .unwrap_or_default();
        let name = if agent.is_empty() {
            "Agent".to_owned()
        } else {
            agent_label(&agent)
        };
        let item = format!("#{} {}", t.number, t.title);
        let (title, pill, tint) = match t.status {
            TicketStatus::Working => (format!("{name} working: {item}"), "working", 0x7C_8CFF),
            TicketStatus::NeedsApproval => (
                format!("{name} needs permission: {item}"),
                "waiting",
                0xF5_B14C,
            ),
            TicketStatus::Review if t.shadow => {
                (format!("Proposal ready: {item}"), "proposal", 0x5B_C0FF)
            }
            TicketStatus::Review if t.pending_post => {
                (format!("Draft ready: {item}"), "draft", 0x5B_C0FF)
            }
            TicketStatus::Review => (
                format!("{name} opened a pull request: {item}"),
                "review",
                0x5B_C0FF,
            ),
            TicketStatus::Done => (format!("{name} finished: {item}"), "done", 0x3D_D68C),
            TicketStatus::Failed => (format!("{name} failed: {item}"), "failed", 0xF2_555A),
            TicketStatus::Discarded => (format!("Discarded: {item}"), "", 0x6B_717C),
        };
        let chip_tint = if agent.is_empty() {
            rgb(0x7C_8CFF)
        } else {
            crate::orchestra::agent_color(&agent)
        };
        out.push(Notice {
            row: InboxNotice {
                id: sv(&t.id),
                chip: sv(&initials(&name)),
                tint: chip_tint,
                title: sv(&title),
                meta: sv(&format!("{} · {}", t.repo, ago(t.updated_at))),
                pill: sv(pill),
                pill_tint: rgb(tint),
                trailing: SharedString::new(),
                unread: t.updated_at > s.read_at,
                can_open: t.session_id.is_some(),
            },
            at: t.updated_at,
            subagent: true,
            build: false,
        });
    }
    for r in &s.triggers {
        let pr = r.kind.contains("pull") || r.kind.contains("review");
        out.push(Notice {
            row: InboxNotice {
                id: sv(&r.id),
                chip: sv("i"),
                tint: rgb(0x5B_C0FF),
                title: sv(&format!(
                    "{}#{} {} by {}",
                    r.repo, r.number, r.kind, r.author
                )),
                meta: sv(&format!("{} · {}", r.repo, ago(r.received_at))),
                pill: sv(if !r.accepted {
                    "ignored"
                } else if pr {
                    "review"
                } else {
                    "mention"
                }),
                pill_tint: rgb(if r.accepted { 0x5B_C0FF } else { 0x6B_717C }),
                trailing: SharedString::new(),
                unread: r.received_at > s.read_at,
                can_open: false,
            },
            at: r.received_at,
            subagent: false,
            build: false,
        });
    }
    out.sort_by_key(|n| std::cmp::Reverse(n.at));
    out
}

fn render_notices() {
    let (rows, all, unread, subagents, builds, filter) = STATE.with(|cell| {
        let s = cell.borrow();
        let list = notices(&s);
        let count = |f: &dyn Fn(&Notice) -> bool| {
            i32::try_from(list.iter().filter(|n| f(n)).count()).unwrap_or(i32::MAX)
        };
        let (all, unread, subagents, builds) = (
            count(&|_| true),
            count(&|n| n.row.unread),
            count(&|n| n.subagent),
            count(&|n| n.build),
        );
        let filter = if s.filter.is_empty() {
            "all".to_owned()
        } else {
            s.filter.clone()
        };
        let rows: Vec<InboxNotice> = list
            .into_iter()
            .filter(|n| match filter.as_str() {
                "unread" => n.row.unread,
                "subagents" => n.subagent,
                "builds" => n.build,
                _ => true,
            })
            .map(|n| n.row)
            .collect();
        (rows, all, unread, subagents, builds, filter)
    });
    with_ui(|ui| {
        ui.set_notices(model(rows));
        ui.set_count_all(all);
        ui.set_count_unread(unread);
        ui.set_count_subagents(subagents);
        ui.set_count_builds(builds);
        ui.set_filter(sv(&filter));
    });
}

fn source(id: &str) -> Option<InboxSource> {
    STATE.with(|s| {
        s.borrow()
            .status
            .sources
            .iter()
            .find(|x| x.id == id)
            .cloned()
    })
}

fn input_from(source: &InboxSource) -> InboxSourceInput {
    InboxSourceInput {
        id: Some(source.id.clone()),
        repo: source.repo.clone(),
        workspace_path: source.workspace_path.clone(),
        workspace_key: source.workspace_key.clone(),
        enabled: source.enabled,
        handle: source.handle.clone(),
        allowlist: source.allowlist.clone(),
        interval_secs: source.interval_secs,
        agent: source.agent.clone(),
        draft_first: source.draft_first,
        max_turns: source.max_turns,
        route: source.route,
        shadow_labels: source.shadow_labels.clone(),
    }
}

fn open_form(source: Option<&InboxSource>) {
    let options: Vec<SelectOption> = agent_options()
        .into_iter()
        .map(|(id, label)| SelectOption {
            id: sv(&id),
            label: sv(&label),
        })
        .collect();
    with_ui(|ui| {
        ui.set_agents(model(options));
        ui.set_form_error(SharedString::new());
        match source {
            Some(s) => {
                let agent = s
                    .agent
                    .as_ref()
                    .map(|a| a.as_str().to_owned())
                    .unwrap_or_default();
                ui.set_form_id(sv(&s.id));
                ui.set_form_repo(sv(&s.repo));
                ui.set_form_folder(sv(&s.workspace_path));
                ui.set_form_users(sv(&s.allowlist.join(", ")));
                ui.set_form_handle(sv(&s.handle));
                ui.set_form_agent_label(sv(&agent_label(&agent)));
                ui.set_form_agent(sv(&agent));
                ui.set_form_minutes(i32::try_from(s.interval_secs / 60).unwrap_or(2));
                ui.set_form_draft(s.draft_first);
                ui.set_form_enabled(s.enabled);
                ui.set_form_route(s.route);
                ui.set_form_shadow(sv(&s.shadow_labels.join(", ")));
            }
            None => {
                ui.set_form_id(SharedString::new());
                ui.set_form_repo(SharedString::new());
                ui.set_form_folder(sv(&crate::bridge::workspace_path()));
                ui.set_form_users(SharedString::new());
                ui.set_form_handle(sv(harness_core_api::DEFAULT_INBOX_HANDLE));
                ui.set_form_agent(SharedString::new());
                ui.set_form_agent_label(sv("Default agent"));
                ui.set_form_minutes(2);
                ui.set_form_draft(false);
                ui.set_form_enabled(true);
                ui.set_form_route(false);
                ui.set_form_shadow(SharedString::new());
            }
        }
        ui.set_form_open(true);
    });
}

fn save_form() {
    let Some(w) = window() else {
        return;
    };
    let ui = w.global::<InboxUi>();
    let id = ui.get_form_id().to_string();
    let previous = source(&id);
    let agent = ui.get_form_agent().to_string();
    let minutes = u32::try_from(ui.get_form_minutes().max(1)).unwrap_or(2);
    let input = InboxSourceInput {
        id: (!id.is_empty()).then_some(id),
        repo: ui.get_form_repo().trim().to_owned(),
        workspace_path: ui.get_form_folder().trim().to_owned(),
        workspace_key: previous
            .as_ref()
            .map(|p| p.workspace_key.clone())
            .unwrap_or_default(),
        enabled: ui.get_form_enabled(),
        handle: ui.get_form_handle().trim().to_owned(),
        allowlist: ui
            .get_form_users()
            .split([',', ' ', ';'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
        interval_secs: minutes.saturating_mul(60),
        agent: (!agent.is_empty()).then(|| agent.as_str().into()),
        draft_first: ui.get_form_draft(),
        max_turns: previous.as_ref().map_or(0, |p| p.max_turns),
        route: ui.get_form_route(),
        shadow_labels: ui
            .get_form_shadow()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect(),
    };
    let (reply, answer) = tokio::sync::oneshot::channel();
    crate::bridge::send(Command::Chat(ChatCall::SaveInboxSource(input, reply)));
    let _ = slint::spawn_local(async move {
        match answer.await.unwrap_or(Err(ApiError::ShutDown)) {
            Ok(_) => {
                with_ui(|ui| ui.set_form_open(false));
                refresh();
            }
            Err(error) => with_ui(|ui| ui.set_form_error(sv(&error.to_string()))),
        }
    });
}

fn action_for(action: &str, id: &str) {
    let id = id.to_owned();
    let refreshed = |()| refresh();
    match action {
        "filter" => {
            STATE.with(|s| s.borrow_mut().filter = id.clone());
            render_notices();
        }
        "mark-read" => {
            STATE.with(|s| s.borrow_mut().read_at = now_ms());
            render_notices();
        }
        "open-notice" => action_for("open-session", &id),
        "add" => open_form(None),
        "install-gh" => {
            set_error("");
            with_ui(|ui| ui.set_gh_installing(true));
            crate::bridge::send(Command::Setup(crate::core::SetupCall::Install(
                "gh".to_owned(),
            )));
        }
        "copy-gh" => {
            let command = STATE.with(|s| gh_command(&s.borrow().status));
            if let Some(w) = window() {
                w.invoke_copy_text(sv(&command));
            }
        }
        "edit" => open_form(source(&id).as_ref()),
        "agent-label" => with_ui(|ui| ui.set_form_agent_label(sv(&agent_label(&id)))),
        "enable" | "disable" => {
            if let Some(mut input) = source(&id).as_ref().map(input_from) {
                input.enabled = action == "enable";
                call(
                    |reply| ChatCall::SaveInboxSource(input, reply),
                    |_| refresh(),
                );
            }
        }
        "remove" => call(|reply| ChatCall::DeleteInboxSource(id, reply), refreshed),
        "poll" => {
            with_ui(|ui| ui.set_busy(sv(&id)));
            let (reply, answer) = tokio::sync::oneshot::channel();
            crate::bridge::send(Command::Chat(ChatCall::PollInboxNow(id, reply)));
            let _ = slint::spawn_local(async move {
                let result = answer.await.unwrap_or(Err(ApiError::ShutDown));
                with_ui(|ui| ui.set_busy(SharedString::new()));
                match result {
                    Ok(()) => set_error(""),
                    Err(error) => set_error(&error.to_string()),
                }
                refresh();
            });
        }
        "open-session" => {
            let session = STATE.with(|s| {
                s.borrow()
                    .tickets
                    .iter()
                    .find(|t| t.id == id)
                    .and_then(|t| t.session_id.clone())
            });
            if let Some(session) = session {
                crate::bridge::open_session(session.as_str());
            }
        }
        "open-router" => {
            if let Some(session) = source(&id).and_then(|s| s.router_session_id) {
                crate::bridge::open_session(session.as_str());
            }
        }
        "post" => call(|reply| ChatCall::PostTicket(id, reply), refreshed),
        "stop" => call(|reply| ChatCall::StopTicket(id, reply), refreshed),
        "discard" => call(|reply| ChatCall::DiscardTicket(id, reply), refreshed),
        _ => {}
    }
}
