use derive::Fsm;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Fsm)]
#[fsm(
    role = ::domain::Role,
    terminal = [Done],
    transitions(
        Draft => Done by (Coordinator),
    ),
)]
enum BadFsm {
    Draft(u32),
    Done,
}

fn main() {}
