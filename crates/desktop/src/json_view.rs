use crate::{AMBER, CYAN, INK, MINT, MUTED, mono};
use eframe::egui::{self, Color32, FontId, Id, Response, TextFormat, text::LayoutJob};
use serde_json::Value;
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    ops::Range,
};

const PAGE_SIZE: usize = 32;
const STRING_PAGE_BYTES: usize = 4096;
const PREVIEW_CHARS: usize = 160;
const BOOL: Color32 = Color32::from_rgb(187, 164, 235);

/// Render an independently collapsible tree without formatting unopened descendants.
/// The caller's salt must identify this value within its surrounding UI.
pub(crate) fn json_view(ui: &mut egui::Ui, id_salt: impl Hash, value: &Value) {
    let mut salt = DefaultHasher::new();
    id_salt.hash(&mut salt);
    ui.push_id(salt.finish(), |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        ui.spacing_mut().indent = 14.0;
        ui.spacing_mut().icon_width = 12.0;
        node(ui, ui.scope_id().with("root"), Name::Root, value);
    });
}

#[derive(Clone, Copy)]
enum Name<'a> {
    Root,
    Key(&'a str),
    Index(usize),
}

fn node(ui: &mut egui::Ui, id: Id, name: Name<'_>, value: &Value) {
    let len = match value {
        Value::Array(items) => items.len(),
        Value::Object(items) => items.len(),
        _ => 0,
    };
    if len > 0 {
        let mut label = name_label(name);
        let (open, close, kind) = if value.is_array() {
            ("[", "]", "items")
        } else {
            ("{", "}", "keys")
        };
        append(&mut label, open, INK);
        append(&mut label, &format!(" {len} {kind} "), MUTED);
        append(&mut label, close, INK);
        collapsible(ui, id, name, value, label, |ui| {
            let range = page_controls(ui, id.with("page"), len, PAGE_SIZE, "items");
            match value {
                Value::Array(items) => {
                    for index in range {
                        node(ui, id.with(index), Name::Index(index), &items[index]);
                    }
                }
                Value::Object(items) => {
                    for (key, item) in items.iter().skip(range.start).take(range.len()) {
                        node(ui, id.with(key), Name::Key(key), item);
                    }
                }
                _ => unreachable!(),
            }
        });
        return;
    }

    let mut label = name_label(name);
    let (preview, color, abbreviated) = scalar_preview(value);
    append(&mut label, &preview, color);
    if let Value::String(text) = value
        && abbreviated
    {
        append(&mut label, " ...", MUTED);
        collapsible(ui, id, name, value, label, |ui| {
            let range = page_controls(
                ui,
                id.with("string_page"),
                text.len(),
                STRING_PAGE_BYTES,
                "bytes",
            );
            let range = char_boundary_range(text, range);
            egui::ScrollArea::vertical()
                .id_salt(id.with("string_scroll"))
                .max_height(180.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(mono(&text[range], 12.0, MINT))
                            .wrap()
                            .selectable(true),
                    );
                });
        });
    } else {
        ui.horizontal(|ui| {
            ui.add_space(ui.spacing().indent);
            let response = value_hint(
                ui.add(egui::Label::new(label).truncate().selectable(true)),
                name,
                value,
            );
            copy_menu(&response, name, value);
        });
    }
}

fn collapsible(
    ui: &mut egui::Ui,
    id: Id,
    name: Name<'_>,
    value: &Value,
    label: LayoutJob,
    body: impl FnOnce(&mut egui::Ui),
) {
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        id,
        matches!(name, Name::Root),
    );
    let header = ui.horizontal(|ui| {
        if crate::disclosure(ui, state.is_open(), "Expand or collapse this JSON level").clicked() {
            state.toggle(ui);
        }
        let response = value_hint(
            ui.add(
                egui::Label::new(label)
                    .truncate()
                    .sense(egui::Sense::click()),
            ),
            name,
            value,
        );
        if response.clicked() {
            state.toggle(ui);
        }
        copy_menu(&response, name, value);
    });
    state.show_body_indented(&header.response, ui, body);
}

fn value_hint(response: Response, name: Name<'_>, value: &Value) -> Response {
    response.on_hover_ui(|ui| {
        ui.set_max_width(440.0);
        if let Name::Key(key) = name {
            let (prefix, abbreviated) = text_prefix(key, 512);
            let suffix = if abbreviated { " ..." } else { "" };
            ui.add(
                egui::Label::new(mono(format!("{}{suffix}", quoted(prefix)), 12.0, CYAN)).wrap(),
            );
        }
        if !value.is_object() && !value.is_array() {
            let (preview, color, abbreviated) = scalar_preview(value);
            let suffix = if abbreviated { " ..." } else { "" };
            ui.add(egui::Label::new(mono(format!("{preview}{suffix}"), 12.0, color)).wrap());
        }
        ui.label(mono("Right-click to copy the full value", 10.0, MUTED));
    })
}

fn copy_menu(response: &Response, name: Name<'_>, value: &Value) {
    response.context_menu(|ui| {
        if ui.button("Copy JSON").clicked() {
            if let Ok(json) = serde_json::to_string_pretty(value) {
                ui.ctx().copy_text(json);
            }
            ui.close();
        }
        if let Value::String(text) = value
            && ui.button("Copy text").clicked()
        {
            ui.ctx().copy_text(text.clone());
            ui.close();
        }
        if let Name::Key(key) = name
            && ui.button("Copy key").clicked()
        {
            ui.ctx().copy_text(key.to_owned());
            ui.close();
        }
    });
}

fn page_controls(
    ui: &mut egui::Ui,
    id: Id,
    len: usize,
    page_size: usize,
    unit: &str,
) -> Range<usize> {
    if len <= page_size {
        return 0..len;
    }
    let last_page = (len - 1) / page_size;
    let mut page = ui
        .data(|data| data.get_temp::<usize>(id).unwrap_or(0))
        .min(last_page);
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                page > 0,
                egui::Button::new(mono("<", 11.0, MUTED)).frame(false),
            )
            .on_hover_text("Previous page")
            .clicked()
        {
            page -= 1;
        }
        if ui
            .add_enabled(
                page < last_page,
                egui::Button::new(mono(">", 11.0, MUTED)).frame(false),
            )
            .on_hover_text("Next page")
            .clicked()
        {
            page += 1;
        }
        let range = page_range(len, page, page_size);
        ui.label(mono(
            format!("{}-{} / {len} {unit}", range.start + 1, range.end),
            10.0,
            MUTED,
        ));
    });
    ui.data_mut(|data| data.insert_temp(id, page));
    page_range(len, page, page_size)
}

fn page_range(len: usize, page: usize, page_size: usize) -> Range<usize> {
    let page = page.min(len.saturating_sub(1) / page_size);
    let start = page * page_size;
    start..start.saturating_add(page_size).min(len)
}

// Adjacent byte pages meet on the same UTF-8 boundary, preserving every character.
fn char_boundary_range(text: &str, range: Range<usize>) -> Range<usize> {
    let boundary = |mut offset| {
        while !text.is_char_boundary(offset) {
            offset += 1;
        }
        offset
    };
    boundary(range.start)..boundary(range.end)
}

fn name_label(name: Name<'_>) -> LayoutJob {
    let mut label = LayoutJob::default();
    match name {
        Name::Root => {}
        Name::Key(key) => {
            let (prefix, abbreviated) = text_prefix(key, PREVIEW_CHARS);
            append(&mut label, &quoted(prefix), CYAN);
            if abbreviated {
                append(&mut label, "...", MUTED);
            }
            append(&mut label, ": ", MUTED);
        }
        Name::Index(index) => append(&mut label, &format!("{index}: "), MUTED),
    }
    label
}

fn append(label: &mut LayoutJob, text: &str, color: Color32) {
    label.append(
        text,
        0.0,
        TextFormat {
            font_id: FontId::monospace(12.0),
            color,
            ..Default::default()
        },
    );
}

fn scalar_preview(value: &Value) -> (String, Color32, bool) {
    match value {
        Value::String(text) => {
            let (prefix, abbreviated) = text_prefix(text, PREVIEW_CHARS);
            (quoted(prefix), MINT, abbreviated)
        }
        Value::Number(number) => (number.to_string(), AMBER, false),
        Value::Bool(boolean) => (boolean.to_string(), BOOL, false),
        Value::Null => ("null".into(), MUTED, false),
        Value::Array(_) => ("[]".into(), INK, false),
        Value::Object(_) => ("{}".into(), INK, false),
    }
}

fn quoted(text: &str) -> String {
    serde_json::to_string(text).expect("serializing a JSON string cannot fail")
}

fn text_prefix(text: &str, max_chars: usize) -> (&str, bool) {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => (&text[..end], true),
        None => (text, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_are_bounded_and_escape_json_without_changing_original_text() {
        let original = format!("line\n\t\"{}tail", "🦀".repeat(10_000));
        let value = Value::String(original.clone());
        let (preview, _, abbreviated) = scalar_preview(&value);
        assert!(abbreviated);
        let decoded: String = serde_json::from_str(&preview).unwrap();
        assert_eq!(decoded.chars().count(), PREVIEW_CHARS);
        assert!(original.starts_with(&decoded));
        assert_eq!(value.as_str(), Some(original.as_str()));
        assert_eq!(text_prefix("αβγ", 3), ("αβγ", false));
        assert_eq!(text_prefix("αβγ", 2), ("αβ", true));
    }

    #[test]
    fn pages_cover_wide_arrays_and_unicode_strings_without_losing_data() {
        let len = PAGE_SIZE * 3 + 5;
        let indices = (0..len.div_ceil(PAGE_SIZE))
            .flat_map(|page| page_range(len, page, PAGE_SIZE))
            .collect::<Vec<_>>();
        assert_eq!(indices, (0..len).collect::<Vec<_>>());
        assert_eq!(page_range(0, usize::MAX, PAGE_SIZE), 0..0);
        assert_eq!(page_range(len, usize::MAX, PAGE_SIZE), PAGE_SIZE * 3..len);

        let original = format!("a{}z", "α🦀\n".repeat(2000));
        let reassembled = (0..original.len().div_ceil(STRING_PAGE_BYTES))
            .map(|page| {
                let range = page_range(original.len(), page, STRING_PAGE_BYTES);
                &original[char_boundary_range(&original, range)]
            })
            .collect::<String>();
        assert_eq!(reassembled, original);
    }

    #[test]
    fn closed_children_are_lazy_and_sibling_expansion_is_independent() {
        let ctx = egui::Context::default();
        let value = serde_json::json!({"left": {"deep": {"value": 1}}, "right": {"value": 2}});
        let root_id = Id::unique("tree");
        let other_root_id = Id::unique("other_tree");
        let left_id = root_id.with("left");
        let right_id = root_id.with("right");
        let draw = |ctx: &egui::Context| {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    node(ui, root_id, Name::Root, &value);
                    node(ui, other_root_id, Name::Root, &value);
                });
            });
            output.textures_delta.clear();
        };
        draw(&ctx);
        let load = |id| egui::collapsing_header::CollapsingState::load(&ctx, id);
        assert!(load(root_id).unwrap().is_open());
        assert!(!load(left_id).unwrap().is_open());
        assert!(!load(right_id).unwrap().is_open());
        assert!(load(left_id.with("deep")).is_none());

        let mut left = load(left_id).unwrap();
        left.set_open(true);
        left.store(&ctx);
        draw(&ctx);
        assert!(load(left_id).unwrap().is_open());
        assert!(!load(right_id).unwrap().is_open());
        assert!(!load(left_id.with("deep")).unwrap().is_open());
        assert!(!load(other_root_id.with("left")).unwrap().is_open());
    }
}
