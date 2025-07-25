use super::{ControllerRequest, CtlrTx};
use anyhow::{Context, Result};
use firezone_bin_shared::BUNDLE_ID;
use firezone_logging::err_with_src;
use std::env;
use tauri::AppHandle;
use winreg::RegKey;
use winreg::enums::*;

pub async fn set_autostart(enabled: bool) -> Result<()> {
    // Get path to the current executable
    let exec_path = env::current_exe().context("Failed to get current executable path")?;
    let exec_path_str = format!("\"{}\"", exec_path.to_string_lossy());

    // Open the registry key for autostart configuration
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let run_key = hkcu
        .open_subkey_with_flags(
            r"Software\Microsoft\Windows\CurrentVersion\Run",
            KEY_READ | KEY_WRITE,
        )
        .context("Failed to open registry key for autostart")?;

    if enabled {
        // Add the application to autostart
        run_key
            .set_value("Firezone", &exec_path_str)
            .context("Failed to add application to autostart registry")?;

        tracing::debug!("Added application to autostart: {}", exec_path_str);
    } else {
        // Remove the application from autostart
        match run_key.delete_value("Firezone") {
            Ok(_) => tracing::debug!("Removed application from autostart"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).context("Failed to remove application from autostart registry");
            }
        }
    }

    Ok(())
}

/// Since clickable notifications don't work on Linux yet, the update text
/// must be different on different platforms
pub(crate) fn show_update_notification(
    _app: &AppHandle,
    ctlr_tx: CtlrTx,
    title: &str,
    download_url: url::Url,
) -> Result<()> {
    show_clickable_notification(
        title,
        "Click here to download the new version.",
        ctlr_tx,
        ControllerRequest::UpdateNotificationClicked(download_url),
    )?;
    Ok(())
}

/// Show a notification in the bottom right of the screen
///
/// May say "Windows Powershell" and have the wrong icon in dev mode
/// See <https://github.com/tauri-apps/tauri/issues/3700>
///
/// TODO: Warn about silent failure if the AppID is not installed:
/// <https://github.com/tauri-apps/winrt-notification/issues/17#issuecomment-1988715694>
pub(crate) fn show_notification(_app: &AppHandle, title: &str, body: &str) -> Result<()> {
    tracing::debug!(?title, ?body, "show_notification");

    tauri_winrt_notification::Toast::new(BUNDLE_ID)
        .title(title)
        .text1(body)
        .show()
        .context("Couldn't show notification")?;

    Ok(())
}

/// Show a notification that signals `Controller` when clicked
///
/// May say "Windows Powershell" and have the wrong icon in dev mode
/// See <https://github.com/tauri-apps/tauri/issues/3700>
///
/// Known issue: If the notification times out and goes into the notification center
/// (the little thing that pops up when you click the bell icon), then we may not get the
/// click signal.
///
/// I've seen this reported by people using Powershell, C#, etc., so I think it might
/// be a Windows bug?
/// - <https://superuser.com/questions/1488763/windows-10-notifications-not-activating-the-associated-app-when-clicking-on-it>
/// - <https://stackoverflow.com/questions/65835196/windows-toast-notification-com-not-working>
/// - <https://answers.microsoft.com/en-us/windows/forum/all/notifications-not-activating-the-associated-app/7a3b31b0-3a20-4426-9c88-c6e3f2ac62c6>
///
/// Firefox doesn't have this problem. Maybe they're using a different API.
pub(crate) fn show_clickable_notification(
    title: &str,
    body: &str,
    tx: CtlrTx,
    req: ControllerRequest,
) -> Result<()> {
    // For some reason `on_activated` is FnMut
    let mut req = Some(req);

    tauri_winrt_notification::Toast::new(BUNDLE_ID)
        .title(title)
        .text1(body)
        .scenario(tauri_winrt_notification::Scenario::Reminder)
        .on_activated(move |_| {
            if let Some(req) = req.take() {
                if let Err(error) = tx.blocking_send(req) {
                    tracing::error!(
                        "User clicked on notification, but we couldn't tell `Controller`: {}",
                        err_with_src(&error)
                    );
                }
            }
            Ok(())
        })
        .show()
        .context("Couldn't show clickable URL notification")?;
    Ok(())
}
