#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum SortOrder {
    Asc,
    Dsc,
}

#[derive(Clone, Default)]
pub struct FileHeader {
    pub unique_vals: Vec<String>,
    pub name: String,
    pub visible: bool,
    pub dtype: Option<String>,
    pub sort: Option<SortOrder>,
    pub sort_dir: Option<bool>,
}
