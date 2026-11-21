use eframe::egui;

pub(super) const THEMES: [&str; 3] = ["system", "light", "dark"];

pub(super) fn apply_preference(ctx: &egui::Context, preference: &str) {
    ctx.set_theme(match preference {
        "light" => egui::ThemePreference::Light,
        "dark" => egui::ThemePreference::Dark,
        _ => egui::ThemePreference::System,
    });
}

pub(super) fn path_field(
    ui: &mut egui::Ui,
    label: &str,
    text: &mut String,
    browse_label: &str,
) -> (egui::Response, egui::Response) {
    path_field_with_hint(ui, label, text, browse_label, "")
}

pub(super) fn path_field_with_hint(
    ui: &mut egui::Ui,
    label: &str,
    text: &mut String,
    browse_label: &str,
    hint: &str,
) -> (egui::Response, egui::Response) {
    ui.horizontal(|ui| {
        if !label.is_empty() {
            ui.label(label);
        }
        let size = egui::vec2(
            ui.available_width().min(720.0),
            ui.spacing().interact_size.y,
        );
        ui.allocate_ui_with_layout(
            size,
            egui::Layout::right_to_left(egui::Align::Center),
            |ui| {
                let browse = ui.button(browse_label);
                let field = ui.add(
                    egui::TextEdit::singleline(text)
                        .align(egui::Align2::LEFT_CENTER)
                        .desired_width(f32::INFINITY)
                        .hint_text(hint),
                );
                (field, browse)
            },
        )
        .inner
    })
    .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_path_leaves_browse_button_inside_scroll_viewport() {
        let ctx = egui::Context::default();
        super::super::install_cjk_font(&ctx);
        apply_preference(&ctx, "dark");
        for width in [480.0, 1120.0] {
            let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 420.0));
            let mut input = egui::RawInput {
                screen_rect: Some(rect),
                ..Default::default()
            };
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .native_pixels_per_point = Some(2.0);
            let mut output = ctx.run_ui(input, |ui| {
                egui::ScrollArea::both().show(ui, |ui| {
                    let mut path = format!("C:/{}", "long-directory/".repeat(80));
                    for (label, browse_label) in [
                        ("项目数据目录", "浏览"),
                        ("AiBurn compatibility directory", "Browse"),
                    ] {
                        let (field, browse) = path_field(ui, label, &mut path, browse_label);
                        assert!(rect.contains_rect(browse.rect));
                        assert!(rect.contains_rect(field.rect));
                        assert!(field.rect.right() < browse.rect.left());
                        assert!(field.rect.width() >= 80.0);
                    }
                });
            });
            output.textures_delta.clear();
        }
    }

    #[test]
    fn native_controls_use_the_requested_theme_at_high_dpi() {
        let ctx = egui::Context::default();
        let has_cjk = super::super::load_cjk_font().is_some();
        if has_cjk {
            super::super::install_cjk_font(&ctx);
        }
        for (preference, theme, label) in [
            ("light", egui::Theme::Light, "Start burn"),
            ("dark", egui::Theme::Dark, "开始烧录"),
        ] {
            apply_preference(&ctx, preference);
            let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(480.0, 420.0));
            let mut input = egui::RawInput {
                screen_rect: Some(rect),
                ..Default::default()
            };
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .native_pixels_per_point = Some(2.0);
            let mut output = ctx.run_ui(input, |ui| {
                let button = ui.button(label);
                assert!(rect.contains_rect(button.rect));
                let default_style = egui::Style::default();
                assert_eq!(
                    ui.spacing().interact_size,
                    default_style.spacing.interact_size
                );
                assert_eq!(
                    ui.spacing().button_padding,
                    default_style.spacing.button_padding
                );
                let native_theme = match theme {
                    egui::Theme::Light => egui::Visuals::light(),
                    egui::Theme::Dark => egui::Visuals::dark(),
                };
                assert_eq!(
                    ui.visuals().widgets.inactive.bg_fill,
                    native_theme.widgets.inactive.bg_fill
                );
                assert_eq!(
                    ui.visuals().widgets.inactive.corner_radius,
                    native_theme.widgets.inactive.corner_radius
                );
                assert_eq!(ui.ctx().theme(), theme);
                assert_eq!(ui.ctx().pixels_per_point(), 2.0);
                if has_cjk {
                    assert!(ui.ctx().fonts_mut(
                        |fonts| fonts.has_glyphs(&egui::FontId::proportional(15.0), label)
                    ));
                }
            });
            output.textures_delta.clear();
        }
    }
}
