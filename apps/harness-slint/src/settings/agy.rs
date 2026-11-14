//! "Antigravity accounts" group of the Agents page. The hub owns the account list and the login swaps; this
//! module mirrors the list into `AgentsUi` and opens the agy login console after a successful `SignIn`.

use std::cell::RefCell;

use harness_core_api as api;
use slint::{ComponentHandle, SharedString};

use super::{model, with_state};
use crate::ui::{AgentsUi, AgyAccountRow, AppWindow};

thread_local! {
    static ORDER: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn send(op: api::AgyAccountOp) {
    crate::bridge::send(crate::core::Command::AgyAccounts(op));
}

pub(super) fn refresh() {
    send(api::AgyAccountOp::List);
}

/// Account list (or the error of the last call) from the hub.
pub fn accounts(sign_in: Option<&str>, result: Result<api::AgyAccounts, String>) {
    let mut console_error = None;
    if sign_in.is_some() {
        if let Err(error) = open_login() {
            console_error = Some(format!("open console: {error}"));
        }
    }
    with_state(|s| {
        let ui = s.window.global::<AgentsUi>();
        match result {
            Ok(list) => {
                ui.set_agy_error(console_error.unwrap_or_default().into());
                ui.set_agy_available(list.available);
                ui.set_agy_live_signed_in(list.live_signed_in);
                ui.set_agy_pending(list.pending.clone().unwrap_or_default().into());
                let rows: Vec<AgyAccountRow> = list
                    .order
                    .iter()
                    .filter_map(|id| list.accounts.iter().find(|a| &a.id == id))
                    .map(|a| AgyAccountRow {
                        id: a.id.clone().into(),
                        name: a.name.clone().into(),
                        signed_in: a.signed_in,
                        active: a.active,
                        exhausted: a.exhausted,
                    })
                    .collect();
                ORDER.with(|o| *o.borrow_mut() = list.order);
                ui.set_agy_accounts(model(rows));
            }
            Err(error) => ui.set_agy_error(error.into()),
        }
    });
}

fn move_up(id: &str) {
    let order = ORDER.with(|o| {
        let mut order = o.borrow().clone();
        if let Some(i) = order.iter().position(|x| x == id).filter(|i| *i > 0) {
            order.swap(i, i - 1);
        }
        order
    });
    send(api::AgyAccountOp::SetOrder { order });
}

pub(super) fn wire(window: &AppWindow) {
    let ui = window.global::<AgentsUi>();
    ui.on_agy_add(|name| {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return;
        }
        with_state(|s| {
            s.window
                .global::<AgentsUi>()
                .set_agy_label(SharedString::new());
        });
        send(api::AgyAccountOp::Add { name });
    });
    ui.on_agy_remove(|id| send(api::AgyAccountOp::Remove { id: id.into() }));
    ui.on_agy_sign_in(|id| send(api::AgyAccountOp::SignIn { id: id.into() }));
    ui.on_agy_use(|id| send(api::AgyAccountOp::Use { id: id.into() }));
    ui.on_agy_import(|| {
        send(api::AgyAccountOp::Import {
            name: "Main".into(),
        });
    });
    ui.on_agy_done(|| send(api::AgyAccountOp::FinishSignIn));
    ui.on_agy_refresh(refresh);
    ui.on_agy_move_up(|id| move_up(&id));
}

/// Opens a console running `agy` so the user signs in; agy stores the login itself.
fn open_login() -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "start", "Antigravity login", "cmd", "/K", "agy"])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
    #[cfg(not(windows))]
    {
        Err(std::io::Error::other(
            "Antigravity accounts are only supported on Windows",
        ))
    }
}
