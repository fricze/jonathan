use arboard::Clipboard;
use iocraft::prelude::*;
use std::cmp;

#[derive(Clone)]
struct User {
    id: i32,
    name: String,
    email: String,
}

impl User {
    fn new(id: i32, name: &str, email: &str) -> Self {
        Self {
            id,
            name: name.to_string(),
            email: email.to_string(),
        }
    }
}

#[derive(Default, Props)]
struct UsersTableProps<'a> {
    users: Option<&'a Vec<User>>,
}

#[component]
fn UsersTable<'a>(mut hooks: Hooks, props: &UsersTableProps<'a>) -> impl Into<AnyElement<'a>> {
    let length = props.users.as_ref().map_or(0, |users| users.len());

    let mut clipboard = Clipboard::new().unwrap();

    let mut system = hooks.use_context_mut::<SystemContext>();

    let (width, height) = hooks.use_terminal_size();
    let mut selected_rows = hooks.use_state(|| (0, 0));
    let mut should_exit = hooks.use_state(|| false);

    if should_exit.get() {
        system.exit();
    }

    let users = match props.users {
        Some(users) => users.clone(),
        None => vec![],
    };

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
                        let val = users
                            .clone()
                            .into_iter()
                            .skip(up)
                            .take(down - up + 1)
                            .map(|user| user.name + " " + &user.email)
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
                View(width: 20pct, justify_content: JustifyContent::End, padding_right: 2) {
                    Text(content: "Id", weight: Weight::Bold, decoration: TextDecoration::Underline)
                }

                View(width: 20pct) {
                    Text(content: "Name", weight: Weight::Bold, decoration: TextDecoration::Underline)
                }

                View(width: 20pct) {
                    Text(content: "Email", weight: Weight::Bold, decoration: TextDecoration::Underline)
                }
            }

            #(props.users.map(|users| users.iter().enumerate().map(|(i, user)| element! {
                View(background_color: if i >= selected_rows.get().0 && i <= selected_rows.get().1 { None } else { Some(Color::DarkGrey) }) {
                    View(width: 20pct, justify_content: JustifyContent::End, padding_right: 2) {
                        Text(content: user.id.to_string())
                    }

                    View(width: 20pct) {
                        Text(content: user.name.clone())
                    }

                    View(width: 20pct) {
                        Text(content: user.email.clone())
                    }
                }
            })).into_iter().flatten())
        }
    }
}

fn main() {
    let users = vec![
        User::new(1, "Alice", "alice@example.com"),
        User::new(2, "Bob", "bob@example.com"),
        User::new(3, "Charlie", "charlie@example.com"),
        User::new(4, "David", "david@example.com"),
        User::new(5, "Eve", "eve@example.com"),
        User::new(6, "Frank", "frank@example.com"),
        User::new(7, "Grace", "grace@example.com"),
        User::new(8, "Heidi", "heidi@example.com"),
    ];

    // element!(UsersTable(users: &users)).print();

    smol::block_on(element!(UsersTable(users: &users)).fullscreen()).unwrap();
}
