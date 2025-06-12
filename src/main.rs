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

#[component]
fn CsvTable(mut hooks: Hooks, props: &CsvTableProps) -> impl Into<AnyElement<'static>> {
    let length = props.data.len();

    let mut clipboard = Clipboard::new().unwrap();

    let mut system = hooks.use_context_mut::<SystemContext>();

    let (width, height) = hooks.use_terminal_size();
    let mut selected_rows = hooks.use_state(|| (0, 0));
    let mut should_exit = hooks.use_state(|| false);

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
                    KeyCode::Up => {
                        if up > 0 {
                            if shift_pressed {
                                selected_rows.set((cmp::max(up - 1, 0), down));
                            } else if alt_pressed {
                                selected_rows.set((up, cmp::max(down - 1, up)));
                            } else {
                                selected_rows.set((cmp::max(up - 1, 0), down - 1));
                            }
                        }
                    }
                    KeyCode::Down => {
                        if shift_pressed {
                            selected_rows.set((cmp::min(up + 1, down), down));
                        } else if alt_pressed {
                            selected_rows.set((up, cmp::min(down + 1, length - 1)));
                        } else {
                            selected_rows.set((up + 1, cmp::min(down + 1, length - 1)));
                        }
                    }
                    KeyCode::PageUp => {
                        selected_rows.set((up - 3, down - 3));
                    }
                    KeyCode::PageDown => {
                        selected_rows.set((up + 3, down + 3));
                    }
                    // KeyCode::Left => x.set((x.get() as i32 - 1).max(0) as _),
                    // KeyCode::Right => x.set((x.get() + 1).min(AREA_WIDTH - FACE.width() as u32)),
                    _ => {}
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
            height: height - 1,
            width: width,
            border_style: BorderStyle::Round,
            border_color: Color::Cyan,
        ) {
            View(border_style: BorderStyle::Single, border_edges: Edges::Bottom, border_color: Color::Grey) {
                #(props.headers.into_iter().map(|header| element! {
                    View(width: 20pct, justify_content: JustifyContent::End, padding_right: 2) {
                        Text(content: header.to_string(), weight: Weight::Bold, decoration: TextDecoration::Underline)
                    }
                }))
            }

            #(props.data.iter().take(100).enumerate().map(|(i, row)| element! {
                View(background_color: if i >= selected_rows.get().0 && i <= selected_rows.get().1 { None } else { Some(Color::DarkGrey) }) {
                    #(row.iter().map(|cell| element! {
                        View(width: 20pct, justify_content: JustifyContent::End, padding_right: 2) {
                            Text(content: cell.to_string())
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
