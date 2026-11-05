//! Text branding for this independently maintained FilmCraft fork.

/// Draw plain text using the UI font; no upstream trademark artwork is embedded.
pub fn paint_wordmark(ui: &egui::Ui, left_center: egui::Pos2, height: f32, _dark: bool) -> Option<egui::Rect> {
    let size = if height.is_finite() { height.clamp(1.0, 320.0) } else { 20.0 };
    Some(ui.painter().text(left_center, egui::Align2::LEFT_CENTER, "FilmCraft", egui::FontId::proportional(size), ui.visuals().text_color()))
}
