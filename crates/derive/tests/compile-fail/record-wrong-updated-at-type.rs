use derive::Record;

#[derive(Record)]
struct WrongType {
    id: String,
    updated_at: u64,
}

fn main() {}
