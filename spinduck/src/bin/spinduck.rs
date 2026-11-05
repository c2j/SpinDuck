//! SpinDuck CLI（研发工具壳，§2.1）：仅渲染 core 公共 API，零引擎逻辑。
fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut out = std::io::stdout();
    let code = spinduck_core::cli::run::run(&argv, &mut out);
    std::process::exit(code);
}
