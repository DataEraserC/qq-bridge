//! QSH 内核可执行入口。

use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!("qsh：内核骨架，尚无连接与投递实现");
    ExitCode::from(2)
}
