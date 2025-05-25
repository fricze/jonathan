use std::io::stdout;

use crate::terminal::EnterAlternateScreen;
use crate::terminal::LeaveAlternateScreen;

use clap::Parser;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute, terminal,
};
use csv::Reader;
use csv::StringRecord;
use tui::{
    Terminal,
    backend::CrosstermBackend,
    layout::Constraint,
    style::Color,
    style::Modifier,
    style::Style,
    widgets::{Block, Borders, Row, Table},
};

/// CSV TUI Viewer
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Path to the CSV file
    file: String,
}

mod state;

fn read_csv(path: &str) -> csv::Result<(Vec<Vec<String>>, StringRecord)> {
    let mut rdr = Reader::from_path(path)?;
    let mut rows = vec![];

    let headers = rdr.headers()?.clone();

    for result in rdr.records() {
        let record = result?;
        rows.push(record.iter().map(String::from).collect());
    }

    Ok((rows, headers))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let (data, headers) = read_csv(&args.file)?;

    let borrowed = data
        .iter()
        .map(|inner| inner.iter().map(|s| s.as_str()).collect())
        .collect::<Vec<Vec<&str>>>();

    let mut app = state::App::new(borrowed);

    // Setup terminal
    terminal::enable_raw_mode()?;
    let mut stdout = stdout();
    // execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut row_offset = 0;
    let mut col_offset = 0;

    loop {
        terminal.draw(|f| {
            let size = f.size();
            let rows: Vec<Row> = data
                .iter()
                // .skip(row_offset)
                // .take(40)
                .enumerate()
                .map(|(index, r)| {
                    let sliced = r.iter().skip(col_offset).take(size.width as usize / 10);

                    if index % 2 == 0 {
                        Row::new(sliced.cloned().collect::<Vec<String>>())
                            .style(Style::default().fg(Color::White))
                    } else {
                        Row::new(sliced.cloned().collect::<Vec<String>>())
                    }

                    // Row::new(sliced.cloned().collect::<Vec<String>>())
                })
                .collect();

            let table = Table::new(rows)
                .block(Block::default().title("CSV Viewer").borders(Borders::ALL))
                .widths(&[Constraint::Length(10); 10])
                .style(Style::default())
                .header(Row::new(headers.into_iter()))
                .highlight_style(Style::default().add_modifier(Modifier::BOLD))
                .highlight_symbol(">");

            f.render_stateful_widget(table, size, &mut app.state);
        })?;

        if event::poll(std::time::Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('j') | KeyCode::Down => {
                        app.next();
                        // row_offset += 1;
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        app.previous();

                        // if row_offset > 0 {
                        //     row_offset -= 1;
                        // }
                    }
                    KeyCode::Char('l') | KeyCode::Right => col_offset += 1,
                    KeyCode::Char('h') | KeyCode::Left => {
                        if col_offset > 0 {
                            col_offset -= 1;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    // Restore terminal
    terminal::disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}
