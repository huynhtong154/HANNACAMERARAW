use crate::app::ToneCurveTab;
use crate::i18n::{tr, tr_fmt};
use crate::pipeline::{PointCurve, MAX_POINT_CURVE_POINTS};
use crate::ui::theme::{CHANNEL_BLUE, CHANNEL_GREEN, CHANNEL_RED};
use eframe::egui::Align;
use eframe::egui::{self, Color32, Pos2, Sense, Stroke, StrokeKind, Ui};

const CURVE_HEIGHT: f32 = 210.0;
const POINT_RADIUS: f32 = 5.0;
const PICK_RADIUS: f32 = 16.0;
const MIN_POINT_X_GAP: f32 = 0.005;
/// Input and Output use the familiar 0–255 curve coordinate scale. The
/// engine and persisted curves keep their original normalized f32 values.
const CURVE_COORDINATE_SCALE: f32 = 255.0;

/// The curve point being dragged and its offset from the pointer, so it moves
/// with the pointer instead of jumping onto it.
#[derive(Clone, Copy)]
struct GrabbedPoint {
    index: usize,
    offset: [f32; 2],
}

/// The curve editor. With `touch`, a drag that begins away from every point
/// is left to the scroll area around the editor, so the panel scrolls.
pub(crate) fn tone_curve_editor(
    ui: &mut Ui,
    curve: &mut PointCurve,
    curve_color: Color32,
    touch: bool,
) -> bool {
    curve.sanitize();
    let width = ui.available_width().max(1.0);
    let area_sense = if touch {
        Sense::click()
    } else {
        Sense::click_and_drag()
    };
    let (rect, area) = ui.allocate_exact_size(egui::vec2(width, CURVE_HEIGHT), area_sense);
    // On desktop the curve is a single pointer target. Per-point interactive
    // rectangles would intercept clicks even outside the visible handles.
    // Touch retains small handle targets so the surrounding panel can scroll.
    let area_id = area.id;
    let selection_id = area_id.with("selected-point");
    let mut selected = ui
        .data(|data| data.get_temp::<usize>(selection_id))
        .filter(|index| *index < curve.len as usize);
    let response = if touch {
        (0..curve.len as usize).fold(area, |response, index| {
            let hit = egui::Rect::from_center_size(
                curve_to_screen(rect, curve.points[index]),
                egui::Vec2::splat(PICK_RADIUS * 2.0),
            )
            .intersect(rect);
            response.union(ui.interact(
                hit,
                area_id.with(("curve-point", index)),
                Sense::click_and_drag(),
            ))
        })
    } else {
        area
    };
    let mut changed = false;
    // The point is chosen where the press began and kept for the whole drag.
    // Re-picking the nearest point each frame dropped it whenever the pointer
    // outran it (e.g. while it was pinned at an edge or a neighbor) or
    // switched to a neighbor passed on the way.
    let drag_id = response.id.with("dragged-point");
    if response.drag_started() {
        let grabbed = ui
            .input(|input| input.pointer.press_origin())
            .and_then(|origin| {
                let index = nearest_point(curve, rect, origin, PICK_RADIUS)?;
                let pointer = screen_to_curve(rect, origin);
                let point = curve.points[index];
                Some(GrabbedPoint {
                    index,
                    offset: [point[0] - pointer[0], point[1] - pointer[1]],
                })
            });
        ui.data_mut(|data| match grabbed {
            Some(grabbed) => {
                data.insert_temp(drag_id, grabbed);
            }
            None => {
                data.remove::<GrabbedPoint>(drag_id);
            }
        });
        if let Some(grabbed) = grabbed {
            selected = Some(grabbed.index);
            ui.memory_mut(|memory| memory.request_focus(area_id));
        }
    }
    let grabbed = response
        .dragged()
        .then(|| ui.data(|data| data.get_temp::<GrabbedPoint>(drag_id)))
        .flatten()
        .filter(|grabbed| grabbed.index < curve.len as usize);
    if let (Some(pointer), Some(GrabbedPoint { index, offset })) =
        (response.interact_pointer_pos(), grabbed)
    {
        let pointer = screen_to_curve(rect, pointer);
        changed |= move_point(
            curve,
            index,
            [pointer[0] + offset[0], pointer[1] + offset[1]],
        );
    }
    if response.drag_stopped() {
        ui.data_mut(|data| data.remove::<GrabbedPoint>(drag_id));
    }

    let double_clicked = if touch {
        moduwu_design::double_tapped(&response)
    } else {
        response.double_clicked()
    };
    if response.clicked() || double_clicked || response.secondary_clicked() {
        if let Some(pointer) = response.interact_pointer_pos() {
            ui.memory_mut(|memory| memory.request_focus(area_id));
            let nearest = nearest_point(curve, rect, pointer, PICK_RADIUS);
            if response.secondary_clicked() || (!touch && double_clicked) {
                if let Some(index) = nearest {
                    selected = Some(index);
                    if remove_point(curve, index) {
                        selected = None;
                        changed = true;
                    }
                }
            } else if let Some(index) = nearest {
                selected = Some(index);
            } else if !touch || double_clicked {
                let point = screen_to_curve(rect, pointer);
                if insert_point(curve, point) {
                    selected = curve.points[..curve.len as usize]
                        .iter()
                        .position(|existing| existing[0] == point[0]);
                    changed = true;
                }
            }
        }
    }

    if ui.memory(|memory| memory.has_focus(area_id)) {
        if let Some(index) = selected {
            let (delta, delete, escape) = ui.input_mut(|input| {
                let accelerated = input.modifiers.shift;
                let modifiers = if accelerated {
                    egui::Modifiers::SHIFT
                } else {
                    egui::Modifiers::NONE
                };
                let step = if accelerated { 10.0 } else { 1.0 } / CURVE_COORDINATE_SCALE;
                let horizontal =
                    f32::from(u8::from(
                        input.consume_key(modifiers, egui::Key::ArrowRight),
                    )) - f32::from(u8::from(input.consume_key(modifiers, egui::Key::ArrowLeft)));
                let vertical =
                    f32::from(u8::from(input.consume_key(modifiers, egui::Key::ArrowUp)))
                        - f32::from(u8::from(input.consume_key(modifiers, egui::Key::ArrowDown)));
                (
                    [horizontal * step, vertical * step],
                    input.consume_key(egui::Modifiers::NONE, egui::Key::Delete)
                        || input.consume_key(egui::Modifiers::NONE, egui::Key::Backspace),
                    input.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
                )
            });
            if delete && remove_point(curve, index) {
                selected = None;
                changed = true;
            } else if delta != [0.0, 0.0] {
                let point = curve.points[index];
                changed |= move_point(curve, index, [point[0] + delta[0], point[1] + delta[1]]);
            }
            if escape {
                ui.memory_mut(|memory| memory.surrender_focus(area_id));
            }
        }
        if selected.is_none()
            && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            ui.memory_mut(|memory| memory.surrender_focus(area_id));
        }
    }

    response.widget_info(|| {
        let mut info =
            egui::WidgetInfo::labeled(egui::WidgetType::Other, ui.is_enabled(), tr("Tone curve"));
        info.current_text_value = selected.map(|index| {
            let point = curve.points[index];
            tr_fmt(
                "Input {input}, Output {output}",
                &[
                    (
                        "input",
                        &format!("{:.2}", point[0] * CURVE_COORDINATE_SCALE),
                    ),
                    (
                        "output",
                        &format!("{:.2}", point[1] * CURVE_COORDINATE_SCALE),
                    ),
                ],
            )
        });
        info
    });
    if let Some(index) = selected {
        changed |= point_coordinates(ui, curve, index);
    }
    curve.sanitize();
    ui.data_mut(|data| match selected {
        Some(index) => {
            data.insert_temp(selection_id, index);
        }
        None => data.remove::<usize>(selection_id),
    });

    // Paint after edits, so the selected handle and the curve follow the value
    // that is submitted to the existing preview scheduler in this frame.
    let painter = ui.painter_at(rect);
    let overlay_painter = ui.painter().with_clip_rect(ui.clip_rect());
    let visuals = ui.visuals();
    painter.rect_filled(rect, 4.0, visuals.extreme_bg_color);
    painter.rect_stroke(
        rect,
        4.0,
        Stroke::new(1.0, visuals.widgets.noninteractive.bg_stroke.color),
        StrokeKind::Inside,
    );
    for step in 1..4 {
        let t = step as f32 / 4.0;
        let x = egui::lerp(rect.left()..=rect.right(), t);
        let y = egui::lerp(rect.bottom()..=rect.top(), t);
        let grid = Stroke::new(1.0, visuals.faint_bg_color);
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            grid,
        );
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            grid,
        );
    }
    painter.line_segment(
        [
            Pos2::new(rect.left(), rect.bottom()),
            Pos2::new(rect.right(), rect.top()),
        ],
        Stroke::new(1.0, visuals.weak_text_color()),
    );
    let mut previous = curve_to_screen(rect, [0.0, sample_curve(curve, 0.0)]);
    for sample in 1..=128 {
        let x = sample as f32 / 128.0;
        let next = curve_to_screen(rect, [x, sample_curve(curve, x)]);
        overlay_painter.line_segment([previous, next], Stroke::new(2.0, curve_color));
        previous = next;
    }
    for (index, point) in curve.points.iter().take(curve.len as usize).enumerate() {
        let center = curve_to_screen(rect, *point);
        let selected = selected == Some(index);
        let radius = POINT_RADIUS + if selected { 2.0 } else { 0.0 };
        let fill = if selected {
            visuals.selection.bg_fill
        } else {
            Color32::WHITE
        };
        overlay_painter.circle_filled(center, radius, fill);
        overlay_painter.circle_stroke(center, radius, Stroke::new(1.5, curve_color));
    }
    changed
}

fn point_x_bounds(curve: &PointCurve, index: usize) -> (f32, f32) {
    let min = if index == 0 {
        0.0
    } else {
        curve.points[index - 1][0] + MIN_POINT_X_GAP
    };
    let max = if index + 1 == curve.len as usize {
        1.0
    } else {
        curve.points[index + 1][0] - MIN_POINT_X_GAP
    };
    (min, max.max(min))
}

/// Pointer, numeric and keyboard edits share exactly the same bounds.
fn move_point(curve: &mut PointCurve, index: usize, point: [f32; 2]) -> bool {
    if index >= curve.len as usize || !point.iter().all(|value| value.is_finite()) {
        return false;
    }
    let (min, max) = point_x_bounds(curve, index);
    let point = [point[0].clamp(min, max), point[1].clamp(0.0, 1.0)];
    if curve.points[index] == point {
        return false;
    }
    curve.points[index] = point;
    true
}

fn point_coordinates(ui: &mut Ui, curve: &mut PointCurve, index: usize) -> bool {
    let (min, max) = point_x_bounds(curve, index);
    let mut point = curve.points[index].map(|value| value * CURVE_COORDINATE_SCALE);
    let mut changed = false;
    ui.horizontal(|ui| {
        let input_label = ui.label(tr("Input"));
        changed |= ui
            .add(
                moduwu_design::NumberField::new(
                    &mut point[0],
                    min * CURVE_COORDINATE_SCALE..=max * CURVE_COORDINATE_SCALE,
                )
                .speed(1.0)
                .decimals(2)
                .commit_on_finish(true)
                .id_salt(("curve-input", index)),
            )
            .labelled_by(input_label.id)
            .changed();
        let output_label = ui.label(tr("Output"));
        changed |= ui
            .add(
                moduwu_design::NumberField::new(&mut point[1], 0.0..=CURVE_COORDINATE_SCALE)
                    .speed(1.0)
                    .decimals(2)
                    .commit_on_finish(true)
                    .id_salt(("curve-output", index)),
            )
            .labelled_by(output_label.id)
            .changed();
    });
    changed
        && move_point(
            curve,
            index,
            point.map(|value| value / CURVE_COORDINATE_SCALE),
        )
}

fn curve_to_screen(rect: egui::Rect, point: [f32; 2]) -> Pos2 {
    Pos2::new(
        egui::lerp(rect.left()..=rect.right(), point[0]),
        egui::lerp(rect.bottom()..=rect.top(), point[1]),
    )
}

fn screen_to_curve(rect: egui::Rect, point: Pos2) -> [f32; 2] {
    [
        ((point.x - rect.left()) / rect.width()).clamp(0.0, 1.0),
        ((rect.bottom() - point.y) / rect.height()).clamp(0.0, 1.0),
    ]
}

fn nearest_point(
    curve: &PointCurve,
    rect: egui::Rect,
    pointer: Pos2,
    radius: f32,
) -> Option<usize> {
    curve
        .points
        .iter()
        .take(curve.len as usize)
        .enumerate()
        .map(|(index, point)| (index, curve_to_screen(rect, *point).distance(pointer)))
        .filter(|(_, distance)| *distance <= radius)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(index, _)| index)
}

fn insert_point(curve: &mut PointCurve, point: [f32; 2]) -> bool {
    let len = curve.len as usize;
    if !point.iter().all(|value| value.is_finite())
        || len >= MAX_POINT_CURVE_POINTS
        || point[0] <= curve.points[0][0] + MIN_POINT_X_GAP
        || point[0] >= curve.points[len - 1][0] - MIN_POINT_X_GAP
    {
        return false;
    }
    if curve.points[..len]
        .iter()
        .any(|existing| (existing[0] - point[0]).abs() < 0.015)
    {
        return false;
    }

    let insert_at = curve.points[..len]
        .iter()
        .position(|existing| existing[0] > point[0])
        .unwrap_or(len - 1);
    for index in (insert_at..len).rev() {
        curve.points[index + 1] = curve.points[index];
    }
    curve.points[insert_at] = [point[0], point[1].clamp(0.0, 1.0)];
    curve.len += 1;
    true
}

fn remove_point(curve: &mut PointCurve, index: usize) -> bool {
    let len = curve.len as usize;
    if index == 0 || index >= len.saturating_sub(1) || len <= 2 {
        return false;
    }
    for current in index..len - 1 {
        curve.points[current] = curve.points[current + 1];
    }
    curve.points[len - 1] = [1.0, 1.0];
    curve.len -= 1;
    true
}

fn sample_curve(curve: &PointCurve, input: f32) -> f32 {
    let len = curve.len.clamp(2, MAX_POINT_CURVE_POINTS as u32) as usize;
    let x = input.clamp(0.0, 1.0);
    let mut segment = len - 2;
    for index in 0..len - 1 {
        if x <= curve.points[index + 1][0] {
            segment = index;
            break;
        }
    }

    let p0 = curve.points[segment];
    let p1 = curve.points[segment + 1];
    let width = (p1[0] - p0[0]).max(1e-5);
    let t = ((x - p0[0]) / width).clamp(0.0, 1.0);
    let t2 = t * t;
    let t3 = t2 * t;
    let m0 = tangent(curve, segment, len) * width;
    let m1 = tangent(curve, segment + 1, len) * width;
    let y = (2.0 * t3 - 3.0 * t2 + 1.0) * p0[1]
        + (t3 - 2.0 * t2 + t) * m0
        + (-2.0 * t3 + 3.0 * t2) * p1[1]
        + (t3 - t2) * m1;
    y.clamp(p0[1].min(p1[1]), p0[1].max(p1[1]))
}

fn tangent(curve: &PointCurve, index: usize, len: usize) -> f32 {
    if index == 0 {
        return secant(curve.points[0], curve.points[1]);
    }
    if index + 1 >= len {
        return secant(curve.points[len - 2], curve.points[len - 1]);
    }
    let previous = secant(curve.points[index - 1], curve.points[index]);
    let next = secant(curve.points[index], curve.points[index + 1]);
    if previous * next <= 0.0 {
        0.0
    } else {
        2.0 * previous * next / (previous + next)
    }
}

fn secant(a: [f32; 2], b: [f32; 2]) -> f32 {
    (b[1] - a[1]) / (b[0] - a[0]).max(1e-5)
}

pub(crate) struct ToneCurveChannels<'a> {
    pub rgb: &'a mut PointCurve,
    pub red: &'a mut PointCurve,
    pub green: &'a mut PointCurve,
    pub blue: &'a mut PointCurve,
}

/// The channel tabs with their labels and colours.
fn tone_curve_tabs() -> [(ToneCurveTab, &'static str, Color32); 4] {
    [
        (ToneCurveTab::Rgb, tr("RGB"), Color32::WHITE),
        (ToneCurveTab::Red, "R", CHANNEL_RED),
        (ToneCurveTab::Green, "G", CHANNEL_GREEN),
        (ToneCurveTab::Blue, "B", CHANNEL_BLUE),
    ]
}

pub(crate) fn tone_curve_channel_editor(
    ui: &mut Ui,
    mut curves: ToneCurveChannels<'_>,
    selected_tab: &mut ToneCurveTab,
    min_segment_width: f32,
) -> bool {
    let mut changed = false;
    // Touch puts the channel tabs in a rail beside the curve.
    let touch = crate::platform::PlatformCaps::CURRENT.is_mobile();

    if !touch {
        ui.horizontal(|ui| {
            let spacing = ui.spacing().item_spacing.x;
            let segment_width =
                ((ui.available_width() - moduwu_design::TOOLBAR_ICON_EDGE - spacing * 4.0)
                    .max(min_segment_width))
                    / 4.0;
            for (tab, label, color) in tone_curve_tabs() {
                let text = egui::RichText::new(label).color(color);
                if moduwu_design::segmented_button(ui, text, *selected_tab == tab, segment_width)
                    .on_hover_text(tone_curve_description(tab))
                    .clicked()
                {
                    *selected_tab = tab;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if moduwu_design::ghost_icon_button(
                    ui,
                    egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE,
                    moduwu_design::toolbar_icon_size(),
                    tr("Reset the selected tone curve"),
                )
                .clicked()
                {
                    reset_selected_tone_curve(&mut curves, *selected_tab);
                    changed = true;
                }
            });
        });

        {
            let (curve, color) = selected_tone_curve(curves, *selected_tab);
            changed |= ui
                .push_id(tone_curve_id(*selected_tab), |ui| {
                    tone_curve_editor(ui, curve, color, touch)
                })
                .inner;
        }
    } else {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                let control_height = moduwu_design::CONTROL_HEIGHT;
                ui.spacing_mut().item_spacing.y =
                    ((CURVE_HEIGHT - control_height * 5.0) / 4.0).max(0.0);

                for (tab, label, color) in tone_curve_tabs() {
                    let text = egui::RichText::new(label).color(color);
                    if moduwu_design::segmented_button(
                        ui,
                        text,
                        *selected_tab == tab,
                        moduwu_design::TOOLBAR_ICON_EDGE,
                    )
                    .on_hover_text(tone_curve_description(tab))
                    .clicked()
                    {
                        *selected_tab = tab;
                    }
                }

                if moduwu_design::ghost_icon_button(
                    ui,
                    egui_phosphor::regular::ARROW_COUNTER_CLOCKWISE,
                    moduwu_design::toolbar_icon_size(),
                    tr("Reset the selected tone curve"),
                )
                .clicked()
                {
                    reset_selected_tone_curve(&mut curves, *selected_tab);
                    changed = true;
                }
            });

            let (curve, color) = selected_tone_curve(curves, *selected_tab);
            changed |= ui
                .push_id(tone_curve_id(*selected_tab), |ui| {
                    tone_curve_editor(ui, curve, color, touch)
                })
                .inner;
        });
    }

    // Touch has no hover, so the gestures are stated as visible text.
    if touch {
        ui.add(
            egui::Label::new(
                egui::RichText::new(tr(
                    "Double-tap to add a point. Long-press a point to remove it.",
                ))
                .small()
                .color(ui.visuals().weak_text_color()),
            )
            .wrap(),
        );
    }

    changed
}

fn selected_tone_curve(
    curves: ToneCurveChannels<'_>,
    selected_tab: ToneCurveTab,
) -> (&mut PointCurve, Color32) {
    match selected_tab {
        ToneCurveTab::Rgb => (curves.rgb, Color32::WHITE),
        ToneCurveTab::Red => (curves.red, CHANNEL_RED),
        ToneCurveTab::Green => (curves.green, CHANNEL_GREEN),
        ToneCurveTab::Blue => (curves.blue, CHANNEL_BLUE),
    }
}

fn reset_selected_tone_curve(curves: &mut ToneCurveChannels<'_>, selected_tab: ToneCurveTab) {
    match selected_tab {
        ToneCurveTab::Rgb => curves.rgb.reset(),
        ToneCurveTab::Red => curves.red.reset(),
        ToneCurveTab::Green => curves.green.reset(),
        ToneCurveTab::Blue => curves.blue.reset(),
    }
}

fn tone_curve_description(tab: ToneCurveTab) -> &'static str {
    match tab {
        ToneCurveTab::Rgb => tr("Composite luminance curve"),
        ToneCurveTab::Red => tr("Red channel curve"),
        ToneCurveTab::Green => tr("Green channel curve"),
        ToneCurveTab::Blue => tr("Blue channel curve"),
    }
}

/// Keep the selected point, drag and numeric draft local to each channel.
fn tone_curve_id(tab: ToneCurveTab) -> &'static str {
    match tab {
        ToneCurveTab::Rgb => "tone-curve-rgb",
        ToneCurveTab::Red => "tone-curve-red",
        ToneCurveTab::Green => "tone-curve-green",
        ToneCurveTab::Blue => "tone-curve-blue",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_mouse_clicks_add_eight_points_without_hidden_hit_regions() {
        use egui::{Event, Modifiers, PointerButton, RawInput};
        let context = egui::Context::default();
        let mut curve = PointCurve::linear();
        let mut time = 0.0;
        let mut frame = |curve: &mut PointCurve, events: Vec<Event>| {
            time += 0.2;
            let mut changed = false;
            let mut bounds = egui::Rect::NOTHING;
            let _ = context.run_ui(
                RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        Pos2::ZERO,
                        egui::vec2(420.0, 420.0),
                    )),
                    time: Some(time),
                    events,
                    ..RawInput::default()
                },
                |ui| {
                    bounds = egui::Rect::from_min_size(
                        ui.cursor().min,
                        egui::vec2(ui.available_width(), CURVE_HEIGHT),
                    );
                    changed = tone_curve_editor(ui, curve, Color32::WHITE, false);
                },
            );
            (changed, bounds)
        };
        let (_, bounds) = frame(&mut curve, vec![]);
        for index in 0..8 {
            let x = 0.1 + 0.1 * index as f32;
            let position = curve_to_screen(bounds, [x, if index % 2 == 0 { 0.25 } else { 0.75 }]);
            let event = |pressed| Event::PointerButton {
                pos: position,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            };
            frame(&mut curve, vec![Event::PointerMoved(position), event(true)]);
            assert!(
                frame(&mut curve, vec![event(false)]).0,
                "click {} should insert",
                index + 1
            );
            assert_eq!(curve.len as usize, index + 3);
        }
        // Clicking an existing handle selects it; the same pixel cannot add
        // a second point or dispatch another preview edit.
        let existing = curve_to_screen(bounds, curve.points[4]);
        let event = |pressed| Event::PointerButton {
            pos: existing,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        };
        frame(&mut curve, vec![Event::PointerMoved(existing), event(true)]);
        assert!(!frame(&mut curve, vec![event(false)]).0);
        assert_eq!(curve.len, 10);
    }

    #[test]
    fn point_edits_keep_order_endpoints_and_the_engine_limit() {
        let mut curve = PointCurve::linear();
        assert!(insert_point(&mut curve, [0.25, 0.2]));
        assert!(insert_point(&mut curve, [0.75, 0.8]));

        // Both pointer movement and exact coordinates use this edit path.
        assert!(move_point(&mut curve, 1, [240.0 / 255.0, -2.0]));
        assert_eq!(curve.points[1], [0.75 - MIN_POINT_X_GAP, 0.0]);
        assert!(move_point(&mut curve, 2, [-1.0, 280.0 / 255.0]));
        assert!(curve.points[2][0] >= curve.points[1][0] + MIN_POINT_X_GAP);
        assert_eq!(curve.points[2][1], 1.0);
        assert!(!move_point(&mut curve, 1, [f32::NAN, 0.5]));
        assert!(!insert_point(&mut curve, [f32::INFINITY, 0.5]));
        assert!(!remove_point(&mut curve, 0));
        let endpoint = curve.len as usize - 1;
        assert!(!remove_point(&mut curve, endpoint));
        assert!(!remove_point(&mut curve, MAX_POINT_CURVE_POINTS));
        assert!(remove_point(&mut curve, 1));
        curve.sanitize();
        assert_eq!(curve.points[0], [0.0, 0.0]);
        assert_eq!(curve.points[curve.len as usize - 1], [1.0, 1.0]);

        curve.reset();
        for index in 1..MAX_POINT_CURVE_POINTS - 1 {
            let coordinate = index as f32 / (MAX_POINT_CURVE_POINTS - 1) as f32;
            assert!(insert_point(&mut curve, [coordinate, coordinate]));
        }
        assert_eq!(curve.len as usize, MAX_POINT_CURVE_POINTS);
        assert!(!insert_point(&mut curve, [0.51, 0.6]));
        for input in 0..=255 {
            let value = sample_curve(&curve, input as f32 / 255.0);
            assert!(value.is_finite() && (0.0..=1.0).contains(&value));
        }
    }

    #[test]
    fn pointer_and_keyboard_edits_integrate_with_egui_without_a_gpu() {
        use egui::{Event, Key, Modifiers, PointerButton, RawInput};

        let context = egui::Context::default();
        let mut curve = PointCurve::linear();
        let frame = |curve: &mut PointCurve, time: f64, events: Vec<Event>| {
            let mut bounds = egui::Rect::NOTHING;
            let mut editor_id = egui::Id::NULL;
            let mut changed = false;
            let _ = context.run_ui(
                RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        Pos2::ZERO,
                        egui::vec2(400.0, 400.0),
                    )),
                    time: Some(time),
                    events,
                    ..RawInput::default()
                },
                |ui| {
                    bounds = egui::Rect::from_min_size(
                        ui.cursor().min,
                        egui::vec2(ui.available_width(), CURVE_HEIGHT),
                    );
                    editor_id = ui.next_auto_id();
                    changed = tone_curve_editor(ui, curve, Color32::WHITE, false);
                },
            );
            (changed, bounds, editor_id)
        };
        let (_, bounds, editor_id) = frame(&mut curve, 0.0, vec![]);
        let center = curve_to_screen(bounds, [0.5, 0.5]);
        let pointer = |pos, button, pressed| Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: Modifiers::NONE,
        };
        frame(
            &mut curve,
            0.1,
            vec![
                Event::PointerMoved(center),
                pointer(center, PointerButton::Primary, true),
            ],
        );
        assert!(
            frame(
                &mut curve,
                0.2,
                vec![pointer(center, PointerButton::Primary, false)]
            )
            .0
        );
        assert_eq!(curve.len, 3);
        assert_eq!(
            context.data(|data| data.get_temp::<usize>(editor_id.with("selected-point"))),
            Some(1)
        );

        let key = |key, pressed| Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: Modifiers::NONE,
        };
        assert!(frame(&mut curve, 0.3, vec![key(Key::ArrowRight, true)]).0);
        assert!(curve.points[1][0] > 0.5);
        assert_eq!(curve.points[1][1], 0.5);
        assert!(
            frame(
                &mut curve,
                0.4,
                vec![key(Key::ArrowRight, false), key(Key::ArrowUp, true)],
            )
            .0
        );
        assert!(curve.points[1][1] > 0.5);

        let from = curve_to_screen(bounds, curve.points[1]);
        let to = curve_to_screen(bounds, [0.7, 0.8]);
        frame(
            &mut curve,
            1.0,
            vec![
                key(Key::ArrowUp, false),
                Event::PointerMoved(from),
                pointer(from, PointerButton::Primary, true),
            ],
        );
        assert!(frame(&mut curve, 1.1, vec![Event::PointerMoved(to)]).0);
        frame(
            &mut curve,
            1.2,
            vec![pointer(to, PointerButton::Primary, false)],
        );
        assert!((curve.points[1][0] - 0.7).abs() < 0.001);
        assert!((curve.points[1][1] - 0.8).abs() < 0.001);

        // A double click removes only an intermediate point.
        for (time, pressed) in [(2.0, true), (2.05, false), (2.15, true), (2.2, false)] {
            frame(
                &mut curve,
                time,
                vec![pointer(to, PointerButton::Primary, pressed)],
            );
        }
        assert_eq!(curve.len, 2);
        let endpoint = curve_to_screen(bounds, curve.points[0]);
        frame(
            &mut curve,
            3.0,
            vec![
                Event::PointerMoved(endpoint),
                pointer(endpoint, PointerButton::Secondary, true),
            ],
        );
        frame(
            &mut curve,
            3.1,
            vec![pointer(endpoint, PointerButton::Secondary, false)],
        );
        assert_eq!(curve, PointCurve::linear());
    }
}
