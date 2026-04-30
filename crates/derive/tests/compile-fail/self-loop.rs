use derive::Fsm;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Fsm)]
#[fsm(
    role = ::domain::Role,
    terminal = [Done],
    transitions(
        Draft => Draft by (Reactor),
        Draft => Done  by (Reactor),
    ),
)]
enum BadFsm {
    Draft,
    Done,
}

fn main() {}
