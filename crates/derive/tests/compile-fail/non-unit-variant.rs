use derive::Fsm;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Fsm)]
#[fsm(
    role = ::domain::Role,
    terminal = [Done],
    transitions(
        Draft => Done by (Reactor),
    ),
)]
enum BadFsm {
    Draft(u32),
    Done,
}

fn main() {}
