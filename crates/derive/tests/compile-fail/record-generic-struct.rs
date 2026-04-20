use derive::Record;

#[derive(Record)]
struct Generic<T> {
    id: String,
    updated_at: i64,
    value: T,
}

fn main() {}
