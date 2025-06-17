use arboard::Clipboard;
use clap::Parser;
use csv::Reader;

use csv::StringRecord;
use iocraft::prelude::*;
use std::cmp;

#[derive(Default, Props)]
struct CsvTableProps {
    headers: StringRecord,
    data: Vec<StringRecord>,
}

fn get_background(
    select_mode: bool,
    i: usize,
    selected_rows: (usize, usize),
    offset: usize,
) -> Option<Color> {
    if !select_mode {
        return None;
    }

    if i >= selected_rows.0 - offset && i <= selected_rows.1 - offset {
        None
    } else {
        Some(Color::DarkGrey)
    }
}

fn get_color(select_mode: bool, i: usize, selected_rows: (usize, usize), offset: usize) -> Color {
    let diff = selected_rows.1 - selected_rows.0;
    let middle = selected_rows.0 + diff / 2;
    let selected = i == middle - offset;

    if !select_mode {
        if selected {
            Color::Yellow
        } else {
            Color::White
        }
    } else {
        Color::White
    }
}

#[component]
fn CsvTable(mut hooks: Hooks, props: &CsvTableProps) -> impl Into<AnyElement<'static>> {
    let length = props.data.len();

    let mut clipboard = Clipboard::new().unwrap();

    let mut system = hooks.use_context_mut::<SystemContext>();

    let (width, height) = hooks.use_terminal_size();
    let mut selected_rows = hooks.use_state(|| (0, 0));
    let mut should_exit = hooks.use_state(|| false);
    let mut select_mode = hooks.use_state(|| false);
    let mut numbers_pressed = hooks.use_state(|| "".to_string());

    let mut x_offset = hooks.use_state(|| 0 as usize);

    if should_exit.get() {
        system.exit();
    }

    let data = props.data.clone();

    let rows_number = height as usize;

    let scroll_start_distance = rows_number / 2;
    let (up, down) = selected_rows.get();
    let selection_middle = (up + down) / 2;
    let offset = if selection_middle > scroll_start_distance {
        selection_middle - scroll_start_distance
    } else {
        0
    };

    let visible_rows = props.data.iter().skip(0 + offset).take(rows_number);

    let move_by = numbers_pressed.clone().to_string();

    let columns_no = props.headers.len();
    let column_width = 15;

    let visible_columns = columns_no.min(width as usize / column_width);

    let total_width = columns_no * column_width;

    let skip_columns = if total_width > width as usize {
        x_offset.get()
    } else {
        0
    };

    let visible_headers = props
        .headers
        .into_iter()
        .skip(skip_columns)
        .take(visible_columns);

    hooks.use_terminal_events({
        move |event| match event {
            TerminalEvent::Key(KeyEvent {
                modifiers,
                code,
                kind,
                ..
            }) if kind != KeyEventKind::Release => {
                let shift_pressed = modifiers.contains(KeyModifiers::SHIFT);
                let alt_pressed = modifiers.contains(KeyModifiers::ALT);
                // let cmd_pressed = modifiers.contains(KeyModifiers::META);

                let (up, down) = selected_rows.get();

                let current_numbers_str = numbers_pressed.clone().to_string();
                let user_move = current_numbers_str.parse().unwrap_or(1);

                match code {
                    KeyCode::Char('c') => {
                        let val = data
                            .clone()
                            .iter()
                            .skip(up)
                            .take(down - up + 1)
                            .map(|inner| inner.iter().collect::<Vec<&str>>().join(", "))
                            .collect::<Vec<String>>()
                            .join(", ");
                        clipboard.set_text(val).unwrap();
                    }
                    KeyCode::Char('q') => should_exit.set(true),
                    KeyCode::Char('g') => {
                        let distance = down - up;
                        let bottom = user_move + distance;
                        if bottom > length {
                            selected_rows.set((length - distance - 1, length - 1));
                        } else {
                            selected_rows.set((user_move, bottom));
                        }
                    }
                    KeyCode::Char('s') => select_mode.set(!select_mode.get()),
                    KeyCode::Home | KeyCode::Char('u') => {
                        let user_move = current_numbers_str.parse().unwrap_or(0);

                        let new_down = down - up + user_move;
                        selected_rows.set((user_move, new_down));
                    }
                    KeyCode::End | KeyCode::Char('d') => {
                        let user_move = current_numbers_str.parse().unwrap_or(0);

                        let distance = down - up;
                        selected_rows
                            .set((length - distance - 1 - user_move, length - 1 - user_move));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        if up > 0 {
                            if shift_pressed {
                                selected_rows.set((cmp::max(up - user_move, 0), down));
                            } else if alt_pressed {
                                selected_rows.set((up, cmp::max(down - user_move, up)));
                            } else {
                                let distance = down - up;
                                let new_up = up.saturating_sub(user_move);
                                selected_rows.set((new_up, new_up + distance));
                            }
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if shift_pressed {
                            selected_rows.set((cmp::min(up + user_move, down), down));
                        } else if alt_pressed {
                            selected_rows.set((up, cmp::min(down + user_move, length - user_move)));
                        } else {
                            let distance = down - up;
                            let new_down = cmp::min(down + user_move, length - 1);
                            selected_rows.set((new_down - distance, new_down));
                        }
                    }
                    KeyCode::PageUp => {
                        let distance = down - up;
                        let new_up = up.saturating_sub(10);
                        selected_rows.set((new_up, new_up + distance));
                    }
                    KeyCode::PageDown => {
                        let distance = down - up;
                        let new_down = cmp::min(down + 10, length - 1);
                        selected_rows.set((new_down - distance, new_down));
                    }
                    KeyCode::Left => x_offset.set(x_offset.get().saturating_sub(user_move)),
                    KeyCode::Right => {
                        let val = x_offset.get() + user_move;
                        x_offset.set(val.min(columns_no - 1));
                    }
                    _ => {}
                }

                match code {
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        let mut current_numbers_str = numbers_pressed.clone().to_string();
                        current_numbers_str.push(c);
                        numbers_pressed.set(current_numbers_str);
                    }
                    _ => {
                        numbers_pressed.set(String::new());
                    }
                }
            }
            _ => {}
        }
    });

    element! {
        View(
            margin_top: 3,
            margin_bottom: 1,
            flex_direction: FlexDirection::Column,
            flex_wrap: FlexWrap::NoWrap,
            height: height - 1,
            width: width,
        ) {
            View(border_style: BorderStyle::Single, border_edges: Edges::Bottom, border_color: Color::Grey) {
                #(if move_by.is_empty() {
                    element! {
                        View {
                            Text(content: "No move")
                            // Text(content: "Total width: ".to_string() + &total_width.to_string() + &" Width: " + &width.to_string())
                        }
                    }
                } else {
                    element! {
                        View {
                            Text(content: "Move by ".to_string() + &move_by)
                        }
                    }
                })
            }

            View(border_style: BorderStyle::Single, border_edges: Edges::Bottom, border_color: Color::Grey) {
                #(visible_headers.map(|header| element! {
                    View(width: 15, justify_content: JustifyContent::Start, padding_right: 2) {
                        Text(content: header.to_string(), weight: Weight::Bold, decoration: TextDecoration::Underline)
                    }
                }))
            }

            #(visible_rows.enumerate().map(|(i, row)| element! {
                View(background_color: get_background(select_mode.get(), i, selected_rows.get(), offset)) {
                    #(row.iter().skip(skip_columns).take(visible_columns).map(|cell| element! {
                        View(width: 15, justify_content: JustifyContent::Start, padding_right: 2) {
                            Text(content: cell.to_string(), color: get_color(select_mode.get(), i, selected_rows.get(), offset))
                        }
                    }))
                }
            }))
        }
    }
}

fn read_csv(path: &str) -> csv::Result<(Vec<StringRecord>, StringRecord)> {
    let mut rdr = Reader::from_path(path)?;
    let mut rows = vec![];

    let headers = rdr.headers()?.clone();

    for result in rdr.records() {
        let record = result?;
        rows.push(record);
    }

    Ok((rows, headers))
}

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    file: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let (data, headers) = read_csv(&args.file)?;

    smol::block_on(element!(CsvTable(headers: headers, data: data)).fullscreen()).unwrap();

    Ok(())
}
