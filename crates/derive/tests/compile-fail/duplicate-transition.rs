use derive::Fsm;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Fsm)]
#[fsm(
    role = ::domain::Role,
    terminal = [Done],
    transitions(
        Draft => Done by (Coordinator),
        Draft => Done by (Director),
    ),
)]
enum BadFsm {
    Draft,
    Done,
}

fn main() {}
