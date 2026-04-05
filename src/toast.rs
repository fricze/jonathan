use egui::{Context, Id};
use std::time::{Duration, Instant};

const DURATION: Duration = Duration::from_secs(2);

pub fn show(ctx: &Context, message: impl Into<String>) {
    ctx.data_mut(|d| {
        d.insert_temp(Id::new("toast"), (message.into(), Instant::now()));
    });
}

pub fn render(ctx: &Context) {
    let toast: Option<(String, Instant)> = ctx.data(|d| d.get_temp(Id::new("toast")));
    if let Some((msg, at)) = toast {
        if at.elapsed() < DURATION {
            egui::Area::new(Id::new("toast_area"))
                .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0))
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label(msg);
                    });
                });
            ctx.request_repaint();
        } else {
            ctx.data_mut(|d| d.remove::<(String, Instant)>(Id::new("toast")));
        }
    }
}
