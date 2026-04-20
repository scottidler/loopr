use derive::Record;

#[derive(Record)]
#[record(collection = "one")]
#[record(collection = "two")]
struct Dup {
    id: String,
    updated_at: i64,
}

fn main() {}
