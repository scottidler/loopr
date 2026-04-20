use derive::Record;

#[derive(Record)]
struct DupKey {
    id: String,
    updated_at: i64,
    #[record(indexed(key = "same"))]
    a: String,
    #[record(indexed(key = "same"))]
    b: String,
}

fn main() {}
