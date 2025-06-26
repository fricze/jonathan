use csv::Reader;

use csv::StringRecord;

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
