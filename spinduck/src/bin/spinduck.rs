//! SpinDuck CLI（研发工具壳，§2.1）：仅渲染 core 公共 API，零引擎逻辑。
fn main() {
    println!("spinduck {}", spinduck_core::version());
}
