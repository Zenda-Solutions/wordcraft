//! A lost graphics device (the driver reset, `DXGI_ERROR_DEVICE_REMOVED`, a GPU switch) must not
//! take the document with it. wgpu's default error handler panics, so the app used to close with
//! whatever the last recovery copy had. Here errors are logged instead, and when the device is
//! lost the app writes a fresh recovery copy and exits with [`EXIT_GPU_LOST`]: the next launch
//! offers File › Recover, and Zenda Work reopens the tool and restores it on its own.
//!
//! eframe can't recreate a device under a running window, so exiting cleanly is the recovery.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The exit code after a lost device: Work tells it apart from a close (0) and a panic (101).
pub const EXIT_GPU_LOST: i32 = 75;

static LOST: AtomicBool = AtomicBool::new(false);

/// Whether the device was lost and the app should save and leave.
pub fn lost() -> bool {
    LOST.load(Ordering::SeqCst)
}

/// What a device-lost callback does: only a loss the app did not ask for counts.
fn on_lost(reason: eframe::wgpu::DeviceLostReason, message: &str, wake: &dyn Fn()) {
    if reason == eframe::wgpu::DeviceLostReason::Destroyed {
        return;
    }
    log::error!("graphics: device lost ({reason:?}): {message}");
    LOST.store(true, Ordering::SeqCst);
    wake();
}

/// Installs the handlers on the device eframe created. Without wgpu (the GL fallback has a
/// render state too; a build without wgpu has none) there is nothing to install.
pub fn install(cc: &eframe::CreationContext<'_>) {
    let Some(rs) = &cc.wgpu_render_state else { return };
    rs.device.on_uncaptured_error(Arc::new(|e| log::error!("graphics: {e}")));
    let ctx = cc.egui_ctx.clone();
    rs.device.set_device_lost_callback(move |reason, message| on_lost(reason, &message, &|| ctx.request_repaint()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_unasked_loss_makes_the_app_leave() {
        let woke = std::cell::Cell::new(0);
        on_lost(eframe::wgpu::DeviceLostReason::Destroyed, "dropped at exit", &|| woke.set(woke.get() + 1));
        assert!(!lost());
        assert_eq!(woke.get(), 0);
        on_lost(eframe::wgpu::DeviceLostReason::Unknown, "DXGI_ERROR_DEVICE_REMOVED", &|| woke.set(woke.get() + 1));
        assert!(lost());
        assert_eq!(woke.get(), 1);
    }
}
