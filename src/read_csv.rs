use csv::Reader;
use std::fs::File;

use csv::StringRecord;

use crate::ui::FileHeader;

pub fn read_csv(path: &str) -> csv::Result<(Vec<StringRecord>, StringRecord)> {
    let mut rdr = Reader::from_path(path)?;
    let mut rows = vec![];

    let headers = rdr.headers()?.clone();

    for result in rdr.records() {
        let record = result?;
        rows.push(record);
    }

    Ok((rows, headers))
}

pub fn iterate_csv(path: &str) -> csv::Result<(Reader<std::fs::File>, StringRecord)> {
    let mut rdr = Reader::from_path(path)?;

    let headers = rdr.headers()?.clone();

    Ok((rdr, headers))
}

pub fn open_csv_file(path: &str) -> (Reader<File>, Vec<FileHeader>) {
    match iterate_csv(path) {
        Ok((csv_reader, headers)) => {
            let headers = headers
                .into_iter()
                .map(|name| FileHeader {
                    name: name.to_string(),
                    visible: true,
                    ..FileHeader::default()
                })
                .collect::<Vec<_>>();
            return (csv_reader, headers);
        }
        Err(err) => {
            eprintln!("Error reading CSV file: {}", err);
            std::process::exit(1);
        }
    };
}
