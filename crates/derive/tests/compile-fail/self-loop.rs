use derive::Fsm;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Fsm)]
#[fsm(
    role = ::domain::Role,
    terminal = [Done],
    transitions(
        Draft => Draft by (Coordinator),
        Draft => Done  by (Coordinator),
    ),
)]
enum BadFsm {
    Draft,
    Done,
}

fn main() {}
