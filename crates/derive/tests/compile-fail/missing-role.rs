use derive::Fsm;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Fsm)]
#[fsm(
    terminal = [Done],
    transitions(
        Draft => Done by (Reactor),
    ),
)]
enum BadFsm {
    Draft,
    Done,
}

fn main() {}
