use tui::widgets::TableState;

pub struct App<'a> {
    pub state: TableState,
    pub items: Vec<Vec<&'a str>>,
}

impl<'a> App<'a> {
    pub fn new(items: Vec<Vec<&'a str>>) -> App<'a> {
        App {
            state: TableState::default(),
            items,
        }
    }

    pub fn next(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i >= self.items.len() - 10 {
                    0
                } else {
                    i + 10
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    pub fn previous(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.items.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }
}
