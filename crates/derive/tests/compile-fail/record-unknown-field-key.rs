use derive::Record;

#[derive(Record)]
struct BadFieldKey {
    id: String,
    updated_at: i64,
    #[record(indexe)]
    status: String,
}

fn main() {}
