use crate::{BG, CYAN};
use eframe::egui::{self, Stroke, text::CCursorRange};
use std::{hash::Hash, time::Duration};

#[derive(Clone, Default)]
struct CursorBlink {
    focused: bool,
    range: Option<CCursorRange>,
    changed_at: f64,
}

/// A normal editable prompt, with a terminal block at the actual insertion point.
pub(crate) fn prompt_input(
    ui: &mut egui::Ui,
    draft: &mut String,
    id_salt: impl Hash + std::fmt::Debug,
    preview: bool,
) -> egui::Response {
    ui.scope(|ui| {
        // Keep egui's editing, selection and IME handling; replace only its caret.
        ui.visuals_mut().text_cursor.stroke = Stroke::NONE;
        ui.visuals_mut().text_cursor.preview = false;
        let font = egui::FontId::monospace(13.0);
        let output = egui::TextEdit::multiline(draft)
            .font(font.clone())
            .return_key(egui::KeyboardShortcut::new(
                egui::Modifiers::SHIFT,
                egui::Key::Enter,
            ))
            .desired_width(f32::INFINITY)
            .desired_rows(2)
            .frame(egui::Frame::NONE)
            .id_salt(id_salt)
            .show(ui);
        let response = output.response.response;
        let focused = ui.is_enabled() && response.has_focus() && ui.input(|i| i.focused);
        let now = ui.input(|i| i.time);
        let state_id = response.id.with("block_cursor_blink");
        let mut blink = ui.data(|data| data.get_temp::<CursorBlink>(state_id).unwrap_or_default());
        if focused
            && (!blink.focused
                || response.changed()
                || response.clicked()
                || blink.range != output.cursor_range)
        {
            blink.changed_at = now;
        }
        blink.focused = focused;
        blink.range = output.cursor_range;
        let phase = (now - blink.changed_at).max(0.0).rem_euclid(1.0);
        ui.data_mut(|data| data.insert_temp(state_id, blink));

        let cursor = output.cursor_range.and_then(|range| range.single());
        if (focused && cursor.is_some()) || preview {
            if focused {
                let until_transition = if phase < 0.5 {
                    0.5 - phase
                } else {
                    1.0 - phase
                };
                ui.ctx()
                    .request_repaint_after(Duration::from_secs_f64(until_transition));
            }
            if phase < 0.5 || preview {
                let (cell_width, row_height) =
                    ui.fonts_mut(|fonts| (fonts.glyph_width(&font, 'M'), fonts.row_height(&font)));
                let origin = output.galley_pos - egui::vec2(output.galley.rect.left(), 0.0);
                let caret = output.galley.pos_from_cursor(cursor.unwrap_or_default());
                // Empty TextEdit content has a zero-width text clip rect. Use
                // the editable widget's bounds so the full cell remains visible.
                let top_left = if draft.is_empty() {
                    response.rect.min
                } else {
                    origin + caret.min.to_vec2()
                };
                let rect = egui::Rect::from_min_size(top_left, egui::vec2(cell_width, row_height));
                let painter = ui.painter_at(response.rect);
                painter.rect_filled(rect, 0, CYAN);
                if !draft.is_empty() {
                    painter
                        .with_clip_rect(painter.clip_rect().intersect(rect))
                        .galley_with_override_text_color(origin, output.galley, BG);
                }
            }
        }
        response
    })
    .inner
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(
        ctx: &egui::Context,
        draft: &mut String,
        time: f64,
        enabled: bool,
        events: Vec<egui::Event>,
    ) -> (egui::Response, Vec<egui::Rect>) {
        let mut response = None;
        let input = egui::RawInput {
            time: Some(time),
            focused: true,
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(600.0, 200.0),
            )),
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.add_enabled_ui(enabled, |ui| {
                    response = Some(prompt_input(ui, draft, "test_prompt", false));
                });
            });
        });
        fn blocks(shape: &egui::Shape, clip: egui::Rect, result: &mut Vec<egui::Rect>) {
            match shape {
                egui::Shape::Rect(rect) if rect.fill == CYAN => {
                    result.push(rect.rect.intersect(clip))
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        blocks(shape, clip, result);
                    }
                }
                _ => {}
            }
        }
        let mut cursor_blocks = Vec::new();
        for shape in &output.shapes {
            blocks(&shape.shape, shape.clip_rect, &mut cursor_blocks);
        }
        output.textures_delta.clear();
        (response.unwrap(), cursor_blocks)
    }

    #[test]
    fn empty_focused_prompt_paints_a_full_visible_blinking_cell() {
        let ctx = egui::Context::default();
        crate::theme(&ctx);
        let mut text = String::new();
        let (response, blocks) = frame(&ctx, &mut text, 0.0, true, vec![]);
        assert!(blocks.is_empty(), "No active caret without input focus");
        ctx.memory_mut(|memory| memory.request_focus(response.id));
        let (response, blocks) = frame(&ctx, &mut text, 0.1, true, vec![]);
        assert!(response.has_focus());
        assert_eq!(blocks.len(), 1);
        let block = blocks[0];
        assert!(
            block.width() >= 6.0 && block.height() >= 12.0,
            "The empty content clip must not cut off the terminal cell: {block:?}"
        );
        assert!(response.rect.contains_rect(block));
        assert!(frame(&ctx, &mut text, 0.7, true, vec![]).1.is_empty());
        assert_eq!(frame(&ctx, &mut text, 1.2, true, vec![]).1.len(), 1);
        ctx.memory_mut(|memory| memory.surrender_focus(response.id));
        assert!(frame(&ctx, &mut text, 1.3, true, vec![]).1.is_empty());
        ctx.memory_mut(|memory| memory.request_focus(response.id));
        assert_eq!(
            frame(&ctx, &mut text, 1.75, true, vec![]).1.len(),
            1,
            "Regaining focus starts with a visible block, regardless of the old blink phase"
        );
        assert!(frame(&ctx, &mut text, 1.8, false, vec![]).1.is_empty());
    }

    #[test]
    fn block_cursor_tracks_multiline_unicode_and_returns_after_clearing() {
        let ctx = egui::Context::default();
        crate::theme(&ctx);
        let mut text = String::new();
        let (response, _) = frame(&ctx, &mut text, 0.0, true, vec![]);
        ctx.memory_mut(|memory| memory.request_focus(response.id));
        let (_, first) = frame(&ctx, &mut text, 0.1, true, vec![]);
        let (_, typed) = frame(
            &ctx,
            &mut text,
            0.8,
            true,
            vec![egui::Event::Text("hé\nx".into())],
        );
        assert_eq!(text, "hé\nx");
        assert_eq!(typed.len(), 1, "Typing restarts the visible blink phase");
        assert!(typed[0].top() > first[0].top());
        assert!(typed[0].left() > first[0].left());
        let mut state = egui::TextEdit::load_state(&ctx, response.id).unwrap();
        state
            .cursor
            .set_char_range(Some(CCursorRange::select_all_str(&text)));
        state.store(&ctx, response.id);
        assert!(
            frame(&ctx, &mut text, 0.9, true, vec![]).1.is_empty(),
            "Selection stays visible without an overlaid block"
        );
        let (_, cleared) = frame(
            &ctx,
            &mut text,
            1.0,
            true,
            vec![egui::Event::Key {
                key: egui::Key::Backspace,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(text.is_empty());
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0], first[0]);
    }
}
