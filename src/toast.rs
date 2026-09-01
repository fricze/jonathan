use egui::{Context, Id};
use std::time::{Duration, Instant};

const DURATION: Duration = Duration::from_secs(2);
const MIN_WIDTH: f32 = 220.0;

/// A toast is a plain message, or a title plus a body shown below it
/// (e.g. "Undo Eye color" / "\"dupsksko\" → \"blue\"").
#[derive(Clone)]
enum ToastContent {
    Plain(String),
    Titled { title: String, body: String },
}

pub fn show(ctx: &Context, message: impl Into<String>) {
    ctx.data_mut(|d| {
        d.insert_temp(
            Id::new("toast"),
            (ToastContent::Plain(message.into()), Instant::now()),
        );
    });
}

/// Show a toast with a bold title line and a body line below it, e.g. for
/// undo/redo: title = "Undo Eye color", body = "\"dupsksko\" → \"blue\"".
pub fn show_titled(ctx: &Context, title: impl Into<String>, body: impl Into<String>) {
    ctx.data_mut(|d| {
        d.insert_temp(
            Id::new("toast"),
            (
                ToastContent::Titled { title: title.into(), body: body.into() },
                Instant::now(),
            ),
        );
    });
}

pub fn render(ctx: &Context) {
    let toast: Option<(ToastContent, Instant)> = ctx.data(|d| d.get_temp(Id::new("toast")));
    if let Some((content, at)) = toast {
        if at.elapsed() < DURATION {
            egui::Area::new(Id::new("toast_area"))
                .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0))
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(MIN_WIDTH);
                        ui.set_max_width(MIN_WIDTH * 1.6);
                        match content {
                            ToastContent::Plain(msg) => {
                                ui.label(msg);
                            }
                            ToastContent::Titled { title, body } => {
                                ui.vertical(|ui| {
                                    ui.label(egui::RichText::new(title).strong());
                                    ui.add_space(2.0);
                                    ui.label(body);
                                });
                            }
                        }
                    });
                });
            ctx.request_repaint();
        } else {
            ctx.data_mut(|d| d.remove::<(ToastContent, Instant)>(Id::new("toast")));
        }
    }
}
