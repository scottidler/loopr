use derive::Record;

#[derive(Record)]
#[record(collecton = "plans")]
struct Typo {
    id: String,
    updated_at: i64,
}

fn main() {}
