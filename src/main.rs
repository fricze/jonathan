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

    if should_exit.get() {
        system.exit();
    }

    let data = props.data.clone();

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
                    KeyCode::Char('s') => select_mode.set(!select_mode.get()),
                    KeyCode::Home | KeyCode::Char('u') => {
                        let current_numbers_str = numbers_pressed.clone().to_string();
                        let move_by = current_numbers_str.parse().unwrap_or(0);

                        let new_down = down - up + move_by;
                        selected_rows.set((move_by, new_down));
                    }
                    KeyCode::End | KeyCode::Char('d') => {
                        let current_numbers_str = numbers_pressed.clone().to_string();
                        let move_by = current_numbers_str.parse().unwrap_or(0);

                        let distance = down - up;
                        selected_rows.set((length - distance - 1 - move_by, length - 1 - move_by));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        let current_numbers_str = numbers_pressed.clone().to_string();
                        let move_by = current_numbers_str.parse().unwrap_or(1);

                        if up > 0 {
                            if shift_pressed {
                                selected_rows.set((cmp::max(up - move_by, 0), down));
                            } else if alt_pressed {
                                selected_rows.set((up, cmp::max(down - move_by, up)));
                            } else {
                                selected_rows.set((cmp::max(up - move_by, 0), down - move_by));
                            }
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        let current_numbers_str = numbers_pressed.clone().to_string();
                        let move_by = current_numbers_str.parse().unwrap_or(1);

                        if shift_pressed {
                            selected_rows.set((cmp::min(up + move_by, down), down));
                        } else if alt_pressed {
                            selected_rows.set((up, cmp::min(down + move_by, length - move_by)));
                        } else {
                            let distance = down - up;
                            selected_rows.set((
                                cmp::min(up + move_by, length - distance - move_by),
                                cmp::min(down + move_by, length - move_by),
                            ));
                        }
                    }
                    KeyCode::PageUp => {
                        selected_rows.set((cmp::max(up - 10, 0), cmp::max(down - 10, 0)));
                    }
                    KeyCode::PageDown => {
                        let distance = down - up;
                        selected_rows.set((
                            cmp::min(up + 10, length - distance - 1),
                            cmp::min(down + 10, length - 1),
                        ));
                    }
                    // KeyCode::Left => x.set((x.get() as i32 - 1).max(0) as _),
                    // KeyCode::Right => x.set((x.get() + 1).min(AREA_WIDTH - FACE.width() as u32)),
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

    let scroll_start_distance = 20;
    let (up, down) = selected_rows.get();
    let selection_middle = (up + down) / 2;
    let offset = if selection_middle > scroll_start_distance {
        selection_middle - scroll_start_distance
    } else {
        0
    };

    let visible_rows = props.data.iter().skip(0 + offset).take(40);

    let move_by = numbers_pressed.clone().to_string();

    element! {
        View(
            margin_top: 3,
            margin_bottom: 1,
            flex_direction: FlexDirection::Column,
            height: height - 1,
            width: width,
            border_style: BorderStyle::Round,
            border_color: Color::Cyan,
        ) {

            View(border_style: BorderStyle::Single, border_edges: Edges::Bottom, border_color: Color::Grey) {
                #(if move_by.is_empty() {
                    element! {
                        View {
                            Text(content: "No move")
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
                #(props.headers.into_iter().map(|header| element! {
                    View(width: 20pct, justify_content: JustifyContent::End, padding_right: 2) {
                        Text(content: header.to_string(), weight: Weight::Bold, decoration: TextDecoration::Underline)
                    }
                }))
            }

            #(visible_rows.enumerate().map(|(i, row)| element! {
                View(background_color: get_background(select_mode.get(), i, selected_rows.get(), offset)) {
                    #(row.iter().map(|cell| element! {
                        View(width: 20pct, justify_content: JustifyContent::End, padding_right: 2) {
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
    /// Path to the CSV file
    file: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let (data, headers) = read_csv(&args.file)?;

    smol::block_on(element!(CsvTable(headers: headers, data: data)).fullscreen()).unwrap();

    Ok(())
}
