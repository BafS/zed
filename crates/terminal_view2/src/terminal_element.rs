use editor::{Cursor, HighlightedRange, HighlightedRangeLine};
use gpui::{
    black, div, point, px, red, relative, transparent_black, AnyElement, AvailableSpace, Bounds,
    Element, ElementId, FocusHandle, Font, FontStyle, FontWeight, HighlightStyle, Hsla,
    InteractiveElement, InteractiveElementState, IntoElement, LayoutId, ModelContext, Pixels,
    Point, Rgba, ShapedLine, Size, StatefulInteractiveElement, Styled, TextRun, TextStyle,
    TextSystem, UnderlineStyle, WeakModel, WhiteSpace, WindowContext,
};
use itertools::Itertools;
use language::CursorShape;
use settings::Settings;
use terminal::{
    alacritty_terminal::ansi::NamedColor,
    alacritty_terminal::{
        ansi::{Color as AnsiColor, Color::Named, CursorShape as AlacCursorShape},
        grid::Dimensions,
        index::Point as AlacPoint,
        term::{cell::Flags, TermMode},
    },
    terminal_settings::TerminalSettings,
    IndexedCell, Terminal, TerminalContent, TerminalSize,
};
use theme::{ActiveTheme, Theme, ThemeSettings};
use ui::Tooltip;

use std::mem;
use std::{fmt::Debug, ops::RangeInclusive};

///The information generated during layout that is necessary for painting
pub struct LayoutState {
    cells: Vec<LayoutCell>,
    rects: Vec<LayoutRect>,
    relative_highlighted_ranges: Vec<(RangeInclusive<AlacPoint>, Hsla)>,
    cursor: Option<Cursor>,
    background_color: Hsla,
    size: TerminalSize,
    mode: TermMode,
    display_offset: usize,
    hyperlink_tooltip: Option<AnyElement>,
    gutter: Pixels,
}

///Helper struct for converting data between alacritty's cursor points, and displayed cursor points
struct DisplayCursor {
    line: i32,
    col: usize,
}

impl DisplayCursor {
    fn from(cursor_point: AlacPoint, display_offset: usize) -> Self {
        Self {
            line: cursor_point.line.0 + display_offset as i32,
            col: cursor_point.column.0,
        }
    }

    pub fn line(&self) -> i32 {
        self.line
    }

    pub fn col(&self) -> usize {
        self.col
    }
}

#[derive(Debug, Default)]
struct LayoutCell {
    point: AlacPoint<i32, i32>,
    text: gpui::ShapedLine,
}

impl LayoutCell {
    fn new(point: AlacPoint<i32, i32>, text: gpui::ShapedLine) -> LayoutCell {
        LayoutCell { point, text }
    }

    fn paint(
        &self,
        origin: Point<Pixels>,
        layout: &LayoutState,
        _visible_bounds: Bounds<Pixels>,
        cx: &mut WindowContext,
    ) {
        let pos = {
            let point = self.point;

            Point::new(
                (origin.x + point.column as f32 * layout.size.cell_width).floor(),
                origin.y + point.line as f32 * layout.size.line_height,
            )
        };

        self.text.paint(pos, layout.size.line_height, cx).ok();
    }
}

#[derive(Clone, Debug, Default)]
struct LayoutRect {
    point: AlacPoint<i32, i32>,
    num_of_cells: usize,
    color: Hsla,
}

impl LayoutRect {
    fn new(point: AlacPoint<i32, i32>, num_of_cells: usize, color: Hsla) -> LayoutRect {
        LayoutRect {
            point,
            num_of_cells,
            color,
        }
    }

    fn extend(&self) -> Self {
        LayoutRect {
            point: self.point,
            num_of_cells: self.num_of_cells + 1,
            color: self.color,
        }
    }

    fn paint(&self, origin: Point<Pixels>, layout: &LayoutState, cx: &mut WindowContext) {
        let position = {
            let alac_point = self.point;
            point(
                (origin.x + alac_point.column as f32 * layout.size.cell_width).floor(),
                origin.y + alac_point.line as f32 * layout.size.line_height,
            )
        };
        let size = point(
            (layout.size.cell_width * self.num_of_cells as f32).ceil(),
            layout.size.line_height,
        )
        .into();

        cx.paint_quad(
            Bounds::new(position, size),
            Default::default(),
            self.color,
            Default::default(),
            transparent_black(),
        );
    }
}

///The GPUI element that paints the terminal.
///We need to keep a reference to the view for mouse events, do we need it for any other terminal stuff, or can we move that to connection?
pub struct TerminalElement {
    terminal: WeakModel<Terminal>,
    focus: FocusHandle,
    focused: bool,
    cursor_visible: bool,
    can_navigate_to_selected_word: bool,
    interactivity: gpui::Interactivity,
}

impl InteractiveElement for TerminalElement {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        &mut self.interactivity
    }
}

impl StatefulInteractiveElement for TerminalElement {}

impl TerminalElement {
    pub fn new(
        terminal: WeakModel<Terminal>,
        focus: FocusHandle,
        focused: bool,
        cursor_visible: bool,
        can_navigate_to_selected_word: bool,
    ) -> TerminalElement {
        TerminalElement {
            terminal,
            focused,
            focus,
            cursor_visible,
            can_navigate_to_selected_word,
            interactivity: Default::default(),
        }
    }

    //Vec<Range<AlacPoint>> -> Clip out the parts of the ranges

    fn layout_grid(
        grid: &Vec<IndexedCell>,
        text_style: &TextStyle,
        // terminal_theme: &TerminalStyle,
        text_system: &TextSystem,
        hyperlink: Option<(HighlightStyle, &RangeInclusive<AlacPoint>)>,
        cx: &WindowContext<'_>,
    ) -> (Vec<LayoutCell>, Vec<LayoutRect>) {
        let theme = cx.theme();
        let mut cells = vec![];
        let mut rects = vec![];

        let mut cur_rect: Option<LayoutRect> = None;
        let mut cur_alac_color = None;

        let linegroups = grid.into_iter().group_by(|i| i.point.line);
        for (line_index, (_, line)) in linegroups.into_iter().enumerate() {
            for cell in line {
                let mut fg = cell.fg;
                let mut bg = cell.bg;
                if cell.flags.contains(Flags::INVERSE) {
                    mem::swap(&mut fg, &mut bg);
                }

                //Expand background rect range
                {
                    if matches!(bg, Named(NamedColor::Background)) {
                        //Continue to next cell, resetting variables if necessary
                        cur_alac_color = None;
                        if let Some(rect) = cur_rect {
                            rects.push(rect);
                            cur_rect = None
                        }
                    } else {
                        match cur_alac_color {
                            Some(cur_color) => {
                                if bg == cur_color {
                                    cur_rect = cur_rect.take().map(|rect| rect.extend());
                                } else {
                                    cur_alac_color = Some(bg);
                                    if cur_rect.is_some() {
                                        rects.push(cur_rect.take().unwrap());
                                    }
                                    cur_rect = Some(LayoutRect::new(
                                        AlacPoint::new(
                                            line_index as i32,
                                            cell.point.column.0 as i32,
                                        ),
                                        1,
                                        convert_color(&bg, theme),
                                    ));
                                }
                            }
                            None => {
                                cur_alac_color = Some(bg);
                                cur_rect = Some(LayoutRect::new(
                                    AlacPoint::new(line_index as i32, cell.point.column.0 as i32),
                                    1,
                                    convert_color(&bg, &theme),
                                ));
                            }
                        }
                    }
                }

                //Layout current cell text
                {
                    let cell_text = cell.c.to_string();
                    if !is_blank(&cell) {
                        let cell_style = TerminalElement::cell_style(
                            &cell,
                            fg,
                            bg,
                            theme,
                            text_style,
                            text_system,
                            hyperlink,
                        );

                        let layout_cell = text_system
                            .shape_line(
                                cell_text.into(),
                                text_style.font_size.to_pixels(cx.rem_size()),
                                &[cell_style],
                            )
                            //todo!() Can we remove this unwrap?
                            .unwrap();

                        cells.push(LayoutCell::new(
                            AlacPoint::new(line_index as i32, cell.point.column.0 as i32),
                            layout_cell,
                        ))
                    };
                }
            }

            if cur_rect.is_some() {
                rects.push(cur_rect.take().unwrap());
            }
        }
        (cells, rects)
    }

    // Compute the cursor position and expected block width, may return a zero width if x_for_index returns
    // the same position for sequential indexes. Use em_width instead
    fn shape_cursor(
        cursor_point: DisplayCursor,
        size: TerminalSize,
        text_fragment: &ShapedLine,
    ) -> Option<(Point<Pixels>, Pixels)> {
        if cursor_point.line() < size.total_lines() as i32 {
            let cursor_width = if text_fragment.width == Pixels::ZERO {
                size.cell_width()
            } else {
                text_fragment.width
            };

            //Cursor should always surround as much of the text as possible,
            //hence when on pixel boundaries round the origin down and the width up
            Some((
                point(
                    (cursor_point.col() as f32 * size.cell_width()).floor(),
                    (cursor_point.line() as f32 * size.line_height()).floor(),
                ),
                cursor_width.ceil(),
            ))
        } else {
            None
        }
    }

    ///Convert the Alacritty cell styles to GPUI text styles and background color
    fn cell_style(
        indexed: &IndexedCell,
        fg: terminal::alacritty_terminal::ansi::Color,
        bg: terminal::alacritty_terminal::ansi::Color,
        colors: &Theme,
        text_style: &TextStyle,
        text_system: &TextSystem,
        hyperlink: Option<(HighlightStyle, &RangeInclusive<AlacPoint>)>,
    ) -> TextRun {
        let flags = indexed.cell.flags;
        let fg = convert_color(&fg, &colors);
        let bg = convert_color(&bg, &colors);

        let underline = (flags.intersects(Flags::ALL_UNDERLINES)
            || indexed.cell.hyperlink().is_some())
        .then(|| UnderlineStyle {
            color: Some(fg),
            thickness: Pixels::from(1.0),
            wavy: flags.contains(Flags::UNDERCURL),
        });

        let weight = if flags.intersects(Flags::BOLD | Flags::DIM_BOLD) {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        };

        let style = if flags.intersects(Flags::ITALIC) {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };

        let mut result = TextRun {
            len: indexed.c.len_utf8() as usize,
            color: fg,
            background_color: Some(bg),
            font: Font {
                weight,
                style,
                ..text_style.font()
            },
            underline,
        };

        if let Some((style, range)) = hyperlink {
            if range.contains(&indexed.point) {
                if let Some(underline) = style.underline {
                    result.underline = Some(underline);
                }

                if let Some(color) = style.color {
                    result.color = color;
                }
            }
        }

        result
    }

    fn compute_layout(&self, bounds: Bounds<gpui::Pixels>, cx: &mut WindowContext) -> LayoutState {
        let settings = ThemeSettings::get_global(cx).clone();

        //Setup layout information
        // todo!(Terminal tooltips)
        // let tooltip_style = settings.theme.tooltip.clone();

        let buffer_font_size = settings.buffer_font_size(cx);

        let terminal_settings = TerminalSettings::get_global(cx);
        let font_family = terminal_settings
            .font_family
            .as_ref()
            .map(|string| string.clone().into())
            .unwrap_or(settings.buffer_font.family);

        let font_features = terminal_settings
            .font_features
            .clone()
            .unwrap_or(settings.buffer_font.features.clone());

        let line_height = terminal_settings.line_height.value();
        let font_size = terminal_settings.font_size.clone();

        let font_size =
            font_size.map_or(buffer_font_size, |size| theme::adjusted_font_size(size, cx));

        let settings = ThemeSettings::get_global(cx);
        let theme = cx.theme().clone();

        let link_style = HighlightStyle {
            color: Some(gpui::blue()),
            font_weight: None,
            font_style: None,
            background_color: None,
            underline: Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(gpui::red()),
                wavy: false,
            }),
            fade_out: None,
        };

        let text_style = TextStyle {
            font_family,
            font_features,
            font_size: font_size.into(),
            font_style: FontStyle::Normal,
            line_height: line_height.into(),
            background_color: None,
            white_space: WhiteSpace::Normal,
            // These are going to be overridden per-cell
            underline: None,
            color: theme.colors().text,
            font_weight: FontWeight::NORMAL,
        };

        let text_system = cx.text_system();
        let selection_color = theme.players().local();
        let match_color = theme.colors().search_match_background;
        let gutter;
        let dimensions = {
            let rem_size = cx.rem_size();
            let font_pixels = text_style.font_size.to_pixels(rem_size);
            let line_height = font_pixels * line_height.to_pixels(rem_size);
            let font_id = cx.text_system().font_id(&text_style.font()).unwrap();

            // todo!(do we need to keep this unwrap?)
            let cell_width = text_system
                .advance(font_id, font_pixels, 'm')
                .unwrap()
                .width;
            gutter = cell_width;

            let mut size = bounds.size.clone();
            size.width -= gutter;

            TerminalSize::new(line_height, cell_width, size)
        };

        let search_matches = if let Some(terminal_model) = self.terminal.upgrade() {
            terminal_model.read(cx).matches.clone()
        } else {
            Default::default()
        };

        let background_color = theme.colors().background;
        let terminal_handle = self.terminal.upgrade().unwrap();

        let last_hovered_word = terminal_handle.update(cx, |terminal, cx| {
            terminal.set_size(dimensions);
            terminal.try_sync(cx);
            if self.can_navigate_to_selected_word && terminal.can_navigate_to_selected_word() {
                terminal.last_content.last_hovered_word.clone()
            } else {
                None
            }
        });

        let hyperlink_tooltip = last_hovered_word.clone().map(|hovered_word| {
            div()
                .size_full()
                .id("terminal-element")
                .tooltip(move |cx| Tooltip::text(hovered_word.word.clone(), cx))
        });

        let TerminalContent {
            cells,
            mode,
            display_offset,
            cursor_char,
            selection,
            cursor,
            ..
        } = &terminal_handle.read(cx).last_content;

        // searches, highlights to a single range representations
        let mut relative_highlighted_ranges = Vec::new();
        for search_match in search_matches {
            relative_highlighted_ranges.push((search_match, match_color))
        }
        if let Some(selection) = selection {
            relative_highlighted_ranges
                .push((selection.start..=selection.end, selection_color.cursor));
        }

        // then have that representation be converted to the appropriate highlight data structure

        let (cells, rects) = TerminalElement::layout_grid(
            cells,
            &text_style,
            &cx.text_system(),
            last_hovered_word
                .as_ref()
                .map(|last_hovered_word| (link_style, &last_hovered_word.word_match)),
            cx,
        );

        //Layout cursor. Rectangle is used for IME, so we should lay it out even
        //if we don't end up showing it.
        let cursor = if let AlacCursorShape::Hidden = cursor.shape {
            None
        } else {
            let cursor_point = DisplayCursor::from(cursor.point, *display_offset);
            let cursor_text = {
                let str_trxt = cursor_char.to_string();

                let color = if self.focused {
                    theme.players().local().background
                } else {
                    theme.players().local().cursor
                };

                let len = str_trxt.len();
                cx.text_system()
                    .shape_line(
                        str_trxt.into(),
                        text_style.font_size.to_pixels(cx.rem_size()),
                        &[TextRun {
                            len,
                            font: text_style.font(),
                            color,
                            background_color: None,
                            underline: Default::default(),
                        }],
                    )
                    //todo!(do we need to keep this unwrap?)
                    .unwrap()
            };

            let focused = self.focused;
            TerminalElement::shape_cursor(cursor_point, dimensions, &cursor_text).map(
                move |(cursor_position, block_width)| {
                    let (shape, text) = match cursor.shape {
                        AlacCursorShape::Block if !focused => (CursorShape::Hollow, None),
                        AlacCursorShape::Block => (CursorShape::Block, Some(cursor_text)),
                        AlacCursorShape::Underline => (CursorShape::Underscore, None),
                        AlacCursorShape::Beam => (CursorShape::Bar, None),
                        AlacCursorShape::HollowBlock => (CursorShape::Hollow, None),
                        //This case is handled in the if wrapping the whole cursor layout
                        AlacCursorShape::Hidden => unreachable!(),
                    };

                    Cursor::new(
                        cursor_position,
                        block_width,
                        dimensions.line_height,
                        theme.players().local().cursor,
                        shape,
                        text,
                    )
                },
            )
        };

        //Done!
        LayoutState {
            cells,
            cursor,
            background_color,
            size: dimensions,
            rects,
            relative_highlighted_ranges,
            mode: *mode,
            display_offset: *display_offset,
            hyperlink_tooltip: None, // todo!(tooltips)
            gutter,
        }
    }

    fn generic_button_handler<E>(
        connection: WeakModel<Terminal>,
        origin: Point<Pixels>,
        focus_handle: FocusHandle,
        f: impl Fn(&mut Terminal, Point<Pixels>, &E, &mut ModelContext<Terminal>),
    ) -> impl Fn(&E, &mut WindowContext) {
        move |event, cx| {
            cx.focus(&focus_handle);
            if let Some(conn_handle) = connection.upgrade() {
                conn_handle.update(cx, |terminal, cx| {
                    f(terminal, origin, event, cx);

                    cx.notify();
                })
            }
        }
    }

    fn paint_mouse_listeners(
        self,
        origin: Point<Pixels>,
        mode: TermMode,
        bounds: Bounds<Pixels>,
        cx: &mut WindowContext,
    ) -> Self {
        let focus = self.focus.clone();
        let connection = self.terminal.clone();

        self.on_mouse_down(gpui::MouseButton::Left, {
            let connection = connection.clone();
            let focus = focus.clone();
            move |e, cx| {
                cx.focus(&focus);
                //todo!(context menu)
                // v.context_menu.update(cx, |menu, _cx| menu.delay_cancel());
                if let Some(conn_handle) = connection.upgrade() {
                    conn_handle.update(cx, |terminal, cx| {
                        terminal.mouse_down(&e, origin);

                        cx.notify();
                    })
                }
            }
        })
        .on_drag_event({
            let connection = connection.clone();
            let focus = focus.clone();
            move |e, cx| {
                if focus.is_focused(cx) {
                    if let Some(conn_handle) = connection.upgrade() {
                        conn_handle.update(cx, |terminal, cx| {
                            terminal.mouse_drag(e, origin, bounds);
                            cx.notify();
                        })
                    }
                }
            }
        })
        .on_mouse_up(
            gpui::MouseButton::Left,
            TerminalElement::generic_button_handler(
                connection.clone(),
                origin,
                focus.clone(),
                move |terminal, origin, e, cx| {
                    terminal.mouse_up(&e, origin, cx);
                },
            ),
        )
        .on_click({
            let connection = connection.clone();
            move |e, cx| {
                if e.down.button == gpui::MouseButton::Right {
                    let mouse_mode = if let Some(conn_handle) = connection.upgrade() {
                        conn_handle.update(cx, |terminal, _cx| {
                            terminal.mouse_mode(e.down.modifiers.shift)
                        })
                    } else {
                        // If we can't get the model handle, probably can't deploy the context menu
                        true
                    };
                    if !mouse_mode {
                        //todo!(context menu)
                        // view.deploy_context_menu(e.position, cx);
                    }
                }
            }
        })

        //     .on_move(move |event, _: &mut TerminalView, cx| {
        //         if cx.is_self_focused() {
        //             if let Some(conn_handle) = connection.upgrade() {
        //                 conn_handle.update(cx, |terminal, cx| {
        //                     terminal.mouse_move(&event, origin);
        //                     cx.notify();
        //                 })
        //             }
        //         }
        //     })
        //     .on_scroll(move |event, _: &mut TerminalView, cx| {
        //         if let Some(conn_handle) = connection.upgrade() {
        //             conn_handle.update(cx, |terminal, cx| {
        //                 terminal.scroll_wheel(event, origin);
        //                 cx.notify();
        //             })
        //         }
        //     });

        // // Mouse mode handlers:
        // // All mouse modes need the extra click handlers
        // if mode.intersects(TermMode::MOUSE_MODE) {
        //     region = region
        //         .on_down(
        //             MouseButton::Right,
        //             TerminalElement::generic_button_handler(
        //                 connection,
        //                 origin,
        //                 move |terminal, origin, e, _cx| {
        //                     terminal.mouse_down(&e, origin);
        //                 },
        //             ),
        //         )
        //         .on_down(
        //             MouseButton::Middle,
        //             TerminalElement::generic_button_handler(
        //                 connection,
        //                 origin,
        //                 move |terminal, origin, e, _cx| {
        //                     terminal.mouse_down(&e, origin);
        //                 },
        //             ),
        //         )
        //         .on_up(
        //             MouseButton::Right,
        //             TerminalElement::generic_button_handler(
        //                 connection,
        //                 origin,
        //                 move |terminal, origin, e, cx| {
        //                     terminal.mouse_up(&e, origin, cx);
        //                 },
        //             ),
        //         )
        //         .on_up(
        //             MouseButton::Middle,
        //             TerminalElement::generic_button_handler(
        //                 connection,
        //                 origin,
        //                 move |terminal, origin, e, cx| {
        //                     terminal.mouse_up(&e, origin, cx);
        //                 },
        //             ),
        //         )
        // }

        // cx.scene().push_mouse_region(region);
    }
}

impl Element for TerminalElement {
    type State = InteractiveElementState;

    fn layout(
        &mut self,
        element_state: Option<Self::State>,
        cx: &mut WindowContext<'_>,
    ) -> (LayoutId, Self::State) {
        let (layout_id, interactive_state) =
            self.interactivity
                .layout(element_state, cx, |mut style, cx| {
                    style.size.width = relative(1.).into();
                    style.size.height = relative(1.).into();
                    let layout_id = cx.request_layout(&style, None);

                    layout_id
                });

        (layout_id, interactive_state)
    }

    fn paint(self, bounds: Bounds<Pixels>, state: &mut Self::State, cx: &mut WindowContext<'_>) {
        let mut layout = self.compute_layout(bounds, cx);

        let theme = cx.theme();

        cx.paint_quad(
            bounds,
            Default::default(),
            layout.background_color,
            Default::default(),
            Hsla::default(),
        );
        let origin = bounds.origin + Point::new(layout.gutter, px(0.));

        let this = self.paint_mouse_listeners(origin, layout.mode, bounds, cx);

        this.interactivity
            .paint(bounds, bounds.size, state, cx, |_, _, cx| {
                for rect in &layout.rects {
                    rect.paint(origin, &layout, cx);
                }

                cx.with_z_index(1, |cx| {
                    for (relative_highlighted_range, color) in
                        layout.relative_highlighted_ranges.iter()
                    {
                        if let Some((start_y, highlighted_range_lines)) =
                            to_highlighted_range_lines(relative_highlighted_range, &layout, origin)
                        {
                            let hr = HighlightedRange {
                                start_y, //Need to change this
                                line_height: layout.size.line_height,
                                lines: highlighted_range_lines,
                                color: color.clone(),
                                //Copied from editor. TODO: move to theme or something
                                corner_radius: 0.15 * layout.size.line_height,
                            };
                            hr.paint(bounds, cx);
                        }
                    }
                });

                cx.with_z_index(2, |cx| {
                    for cell in &layout.cells {
                        cell.paint(origin, &layout, bounds, cx);
                    }
                });

                if this.cursor_visible {
                    cx.with_z_index(3, |cx| {
                        if let Some(cursor) = &layout.cursor {
                            cursor.paint(origin, cx);
                        }
                    });
                }

                if let Some(element) = layout.hyperlink_tooltip.take() {
                    let width: AvailableSpace = bounds.size.width.into();
                    let height: AvailableSpace = bounds.size.height.into();
                    element.draw(origin, Size { width, height }, cx)
                }
            });
    }

    // todo!() remove?
    // fn metadata(&self) -> Option<&dyn std::any::Any> {
    //     None
    // }

    // fn debug(
    //     &self,
    //     _: Bounds<Pixels>,
    //     _: &Self::State,
    //     _: &Self::PaintState,
    //     _: &TerminalView,
    //     _: &gpui::ViewContext<TerminalView>,
    // ) -> gpui::serde_json::Value {
    //     json!({
    //         "type": "TerminalElement",
    //     })
    // }

    // fn rect_for_text_range(
    //     &self,
    //     _: Range<usize>,
    //     bounds: Bounds<Pixels>,
    //     _: Bounds<Pixels>,
    //     layout: &Self::State,
    //     _: &Self::PaintState,
    //     _: &TerminalView,
    //     _: &gpui::ViewContext<TerminalView>,
    // ) -> Option<Bounds<Pixels>> {
    //     // Use the same origin that's passed to `Cursor::paint` in the paint
    //     // method bove.
    //     let mut origin = bounds.origin() + point(layout.size.cell_width, 0.);

    //     // TODO - Why is it necessary to move downward one line to get correct
    //     // positioning? I would think that we'd want the same rect that is
    //     // painted for the cursor.
    //     origin += point(0., layout.size.line_height);

    //     Some(layout.cursor.as_ref()?.bounding_rect(origin))
    // }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn element_id(&self) -> Option<ElementId> {
        Some("terminal".into())
    }

    fn into_element(self) -> Self::Element {
        self
    }
}

fn is_blank(cell: &IndexedCell) -> bool {
    if cell.c != ' ' {
        return false;
    }

    if cell.bg != AnsiColor::Named(NamedColor::Background) {
        return false;
    }

    if cell.hyperlink().is_some() {
        return false;
    }

    if cell
        .flags
        .intersects(Flags::ALL_UNDERLINES | Flags::INVERSE | Flags::STRIKEOUT)
    {
        return false;
    }

    return true;
}

fn to_highlighted_range_lines(
    range: &RangeInclusive<AlacPoint>,
    layout: &LayoutState,
    origin: Point<Pixels>,
) -> Option<(Pixels, Vec<HighlightedRangeLine>)> {
    // Step 1. Normalize the points to be viewport relative.
    // When display_offset = 1, here's how the grid is arranged:
    //-2,0 -2,1...
    //--- Viewport top
    //-1,0 -1,1...
    //--------- Terminal Top
    // 0,0  0,1...
    // 1,0  1,1...
    //--- Viewport Bottom
    // 2,0  2,1...
    //--------- Terminal Bottom

    // Normalize to viewport relative, from terminal relative.
    // lines are i32s, which are negative above the top left corner of the terminal
    // If the user has scrolled, we use the display_offset to tell us which offset
    // of the grid data we should be looking at. But for the rendering step, we don't
    // want negatives. We want things relative to the 'viewport' (the area of the grid
    // which is currently shown according to the display offset)
    let unclamped_start = AlacPoint::new(
        range.start().line + layout.display_offset,
        range.start().column,
    );
    let unclamped_end =
        AlacPoint::new(range.end().line + layout.display_offset, range.end().column);

    // Step 2. Clamp range to viewport, and return None if it doesn't overlap
    if unclamped_end.line.0 < 0 || unclamped_start.line.0 > layout.size.num_lines() as i32 {
        return None;
    }

    let clamped_start_line = unclamped_start.line.0.max(0) as usize;
    let clamped_end_line = unclamped_end.line.0.min(layout.size.num_lines() as i32) as usize;
    //Convert the start of the range to pixels
    let start_y = origin.y + clamped_start_line as f32 * layout.size.line_height;

    // Step 3. Expand ranges that cross lines into a collection of single-line ranges.
    //  (also convert to pixels)
    let mut highlighted_range_lines = Vec::new();
    for line in clamped_start_line..=clamped_end_line {
        let mut line_start = 0;
        let mut line_end = layout.size.columns();

        if line == clamped_start_line {
            line_start = unclamped_start.column.0 as usize;
        }
        if line == clamped_end_line {
            line_end = unclamped_end.column.0 as usize + 1; //+1 for inclusive
        }

        highlighted_range_lines.push(HighlightedRangeLine {
            start_x: origin.x + line_start as f32 * layout.size.cell_width,
            end_x: origin.x + line_end as f32 * layout.size.cell_width,
        });
    }

    Some((start_y, highlighted_range_lines))
}

///Converts a 2, 8, or 24 bit color ANSI color to the GPUI equivalent
fn convert_color(fg: &terminal::alacritty_terminal::ansi::Color, theme: &Theme) -> Hsla {
    let colors = theme.colors();
    match fg {
        //Named and theme defined colors
        terminal::alacritty_terminal::ansi::Color::Named(n) => match n {
            NamedColor::Black => colors.terminal_ansi_black,
            NamedColor::Red => colors.terminal_ansi_red,
            NamedColor::Green => colors.terminal_ansi_green,
            NamedColor::Yellow => colors.terminal_ansi_yellow,
            NamedColor::Blue => colors.terminal_ansi_blue,
            NamedColor::Magenta => colors.terminal_ansi_magenta,
            NamedColor::Cyan => colors.terminal_ansi_cyan,
            NamedColor::White => colors.terminal_ansi_white,
            NamedColor::BrightBlack => colors.terminal_ansi_bright_black,
            NamedColor::BrightRed => colors.terminal_ansi_bright_red,
            NamedColor::BrightGreen => colors.terminal_ansi_bright_green,
            NamedColor::BrightYellow => colors.terminal_ansi_bright_yellow,
            NamedColor::BrightBlue => colors.terminal_ansi_bright_blue,
            NamedColor::BrightMagenta => colors.terminal_ansi_bright_magenta,
            NamedColor::BrightCyan => colors.terminal_ansi_bright_cyan,
            NamedColor::BrightWhite => colors.terminal_ansi_bright_white,
            NamedColor::Foreground => colors.text,
            NamedColor::Background => colors.background,
            NamedColor::Cursor => theme.players().local().cursor,

            // todo!(more colors)
            NamedColor::DimBlack => red(),
            NamedColor::DimRed => red(),
            NamedColor::DimGreen => red(),
            NamedColor::DimYellow => red(),
            NamedColor::DimBlue => red(),
            NamedColor::DimMagenta => red(),
            NamedColor::DimCyan => red(),
            NamedColor::DimWhite => red(),
            NamedColor::BrightForeground => red(),
            NamedColor::DimForeground => red(),
        },
        //'True' colors
        terminal::alacritty_terminal::ansi::Color::Spec(rgb) => rgba_color(rgb.r, rgb.g, rgb.b),
        //8 bit, indexed colors
        terminal::alacritty_terminal::ansi::Color::Indexed(i) => {
            get_color_at_index(&(*i as usize), theme)
        }
    }
}

///Converts an 8 bit ANSI color to it's GPUI equivalent.
///Accepts usize for compatibility with the alacritty::Colors interface,
///Other than that use case, should only be called with values in the [0,255] range
pub fn get_color_at_index(index: &usize, theme: &Theme) -> Hsla {
    let colors = theme.colors();

    match index {
        //0-15 are the same as the named colors above
        0 => colors.terminal_ansi_black,
        1 => colors.terminal_ansi_red,
        2 => colors.terminal_ansi_green,
        3 => colors.terminal_ansi_yellow,
        4 => colors.terminal_ansi_blue,
        5 => colors.terminal_ansi_magenta,
        6 => colors.terminal_ansi_cyan,
        7 => colors.terminal_ansi_white,
        8 => colors.terminal_ansi_bright_black,
        9 => colors.terminal_ansi_bright_red,
        10 => colors.terminal_ansi_bright_green,
        11 => colors.terminal_ansi_bright_yellow,
        12 => colors.terminal_ansi_bright_blue,
        13 => colors.terminal_ansi_bright_magenta,
        14 => colors.terminal_ansi_bright_cyan,
        15 => colors.terminal_ansi_bright_white,
        //16-231 are mapped to their RGB colors on a 0-5 range per channel
        16..=231 => {
            let (r, g, b) = rgb_for_index(&(*index as u8)); //Split the index into it's ANSI-RGB components
            let step = (u8::MAX as f32 / 5.).floor() as u8; //Split the RGB range into 5 chunks, with floor so no overflow
            rgba_color(r * step, g * step, b * step) //Map the ANSI-RGB components to an RGB color
        }
        //232-255 are a 24 step grayscale from black to white
        232..=255 => {
            let i = *index as u8 - 232; //Align index to 0..24
            let step = (u8::MAX as f32 / 24.).floor() as u8; //Split the RGB grayscale values into 24 chunks
            rgba_color(i * step, i * step, i * step) //Map the ANSI-grayscale components to the RGB-grayscale
        }
        //For compatibility with the alacritty::Colors interface
        256 => colors.text,
        257 => colors.background,
        258 => theme.players().local().cursor,

        // todo!(more colors)
        259 => red(),                      //style.dim_black,
        260 => red(),                      //style.dim_red,
        261 => red(),                      //style.dim_green,
        262 => red(),                      //style.dim_yellow,
        263 => red(),                      //style.dim_blue,
        264 => red(),                      //style.dim_magenta,
        265 => red(),                      //style.dim_cyan,
        266 => red(),                      //style.dim_white,
        267 => red(),                      //style.bright_foreground,
        268 => colors.terminal_ansi_black, //'Dim Background', non-standard color

        _ => black(),
    }
}

///Generates the rgb channels in [0, 5] for a given index into the 6x6x6 ANSI color cube
///See: [8 bit ansi color](https://en.wikipedia.org/wiki/ANSI_escape_code#8-bit).
///
///Wikipedia gives a formula for calculating the index for a given color:
///
///index = 16 + 36 × r + 6 × g + b (0 ≤ r, g, b ≤ 5)
///
///This function does the reverse, calculating the r, g, and b components from a given index.
fn rgb_for_index(i: &u8) -> (u8, u8, u8) {
    debug_assert!((&16..=&231).contains(&i));
    let i = i - 16;
    let r = (i - (i % 36)) / 36;
    let g = ((i % 36) - (i % 6)) / 6;
    let b = (i % 36) % 6;
    (r, g, b)
}

fn rgba_color(r: u8, g: u8, b: u8) -> Hsla {
    Rgba {
        r: (r as f32 / 255.) as f32,
        g: (g as f32 / 255.) as f32,
        b: (b as f32 / 255.) as f32,
        a: 1.,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use crate::terminal_element::rgb_for_index;

    #[test]
    fn test_rgb_for_index() {
        //Test every possible value in the color cube
        for i in 16..=231 {
            let (r, g, b) = rgb_for_index(&(i as u8));
            assert_eq!(i, 16 + 36 * r + 6 * g + b);
        }
    }
}
