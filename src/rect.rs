use egui::text::LayoutJob;
use egui::{Align2, Color32, FontId, Response, Sense, Ui, Vec2, Widget};

// Define a default padding for our rectangle content
const PADDING: f32 = 8.0;

pub struct HoverableRectangle {
    label: String,
    color: Color32,
    hover_color: Color32,
    text_color: Color32,
    font_id: FontId,
    /// Optional: a fixed max width for the rectangle itself.
    /// If None, it will try to fill available width (minus padding).
    max_rect_width: Option<f32>,
}

impl HoverableRectangle {
    pub fn new(
        label: impl Into<String>,
        color: Color32,
        hover_color: Color32,
        text_color: Color32,
    ) -> Self {
        Self {
            label: label.into(),
            color,
            hover_color,
            text_color,
            font_id: FontId::proportional(12.0), // Default font size
            max_rect_width: None,                // Default to no fixed max width for the rectangle
        }
    }

    // // Builder pattern for more configuration
    // pub fn font(mut self, font_id: FontId) -> Self {
    //     self.font_id = font_id;
    //     self
    // }

    // pub fn max_width(mut self, width: f32) -> Self {
    //     self.max_rect_width = Some(width);
    //     self
    // }
}

impl Widget for HoverableRectangle {
    fn ui(self, ui: &mut Ui) -> Response {
        // 1. Determine the maximum width available for the text content
        // This is the allocated width minus padding on both sides.
        let text_max_width = self
            .max_rect_width
            .unwrap_or_else(|| ui.available_width() - 2.0 * PADDING)
            .max(0.0); // Ensure non-negative

        // 2. Prepare the text layout (Galley) to measure its size when wrapped
        let mut layout_job = LayoutJob::simple(
            self.label.clone(),
            self.font_id.clone(),
            self.text_color,
            text_max_width, // This is key for telling egui where to wrap the text
        );
        // layout_job.wrap.break_on_space = true; // Ensure wrapping breaks on spaces
        layout_job.halign = egui::Align::Center; // Center the text horizontally within its wrapped width

        let galley = ui.fonts(|fonts| fonts.layout_job(layout_job));

        // 3. Calculate the desired size of the rectangle based on the text galley's size
        // Add padding around the text
        let mut desired_size = galley.rect.size() + Vec2::new(2.0 * PADDING, 2.0 * PADDING);

        // Ensure a minimum size for better interactivity, even for very short labels
        let min_interact_size = ui.spacing().interact_size;
        desired_size = desired_size.max(min_interact_size);

        // If a max_rect_width was specified, ensure the allocated width matches it.
        // The height is already adjusted by the galley, so only adjust width here.
        let desired_size = if let Some(mw) = self.max_rect_width {
            Vec2::new(mw, desired_size.y)
        } else {
            desired_size
        };

        // 4. Allocate the space for the widget
        // `Sense::hover()` is used to detect hover events over this allocated area.
        let (rect, response) = ui.allocate_exact_size(desired_size, Sense::hover());

        // Determine the background color based on hover state
        let current_color = if response.hovered() {
            self.hover_color
        } else {
            self.color
        };

        let mut rrect = rect.clone();
        rrect.set_width(ui.available_width());
        // 5. Draw the filled rectangle
        ui.painter().rect_filled(rrect, 4.0, current_color); // 4.0 for a small corner radius

        // 6. Draw the text inside the allocated rectangle
        // We calculate the top-left position for the galley so it's centered within the rect.
        let mut text_pos = rect.left_top();
        text_pos.x = text_pos.x + rect.width() / 2.;
        text_pos.y = text_pos.y + PADDING;
        ui.painter().galley(text_pos, galley, current_color);

        response
    }
}

// --- Example Usage in an egui app's update method ---

// pub fn custom_ui_with_wrapped_label(ui: &mut Ui) {
//     ui.heading("Hoverable Rectangles with Wrapped Labels");

//     ui.add_space(10.0);

//     ui.group(|ui| {
//         ui.add(HoverableRectangle::new(
//             "This is a short label that fits on one line.",
//             Color32::from_rgb(100, 100, 200),
//             Color32::from_rgb(150, 150, 250),
//             Color32::WHITE,
//         ));

//         ui.add_space(10.0);

//         ui.add(HoverableRectangle::new(
//             "This is a much longer label that should wrap automatically when the available width is constrained. Watch how the rectangle's height adjusts.",
//             Color32::from_rgb(200, 100, 100),
//             Color32::from_rgb(250, 150, 150),
//             Color32::BLACK,
//         )
//         .max_width(200.0) // Example: Fix max width to 200 pixels for this instance
//         .font(FontId::proportional(14.0)));

//         ui.add_space(10.0);

//         ui.add(HoverableRectangle::new(
//             "Another example with a very, very long text to demonstrate wrapping and height adjustment without a fixed maximum width for the rectangle itself.",
//             Color32::from_rgb(100, 200, 100),
//             Color32::from_rgb(150, 250, 150),
//             Color32::DARK_BLUE,
//         )
//         .font(FontId::monospace(12.0))); // Example: Monospace font
//     });

//     ui.add_space(20.0);

//     ui.label("Try resizing the window to see how the rectangles with no fixed max_width adapt!");
// }

// To integrate this into your egui application, call `custom_ui_with_wrapped_label`
// from your `eframe::App`'s `update` method, typically inside a panel or window:
//
// impl eframe::App for MyApp {
//     fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
//         egui::CentralPanel::default().show(ctx, |ui| {
//             custom_ui_with_wrapped_label(ui);
//         });
//     }
// }
