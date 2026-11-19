#[cfg(feature = "typescript")]
fn main() {
    use jimmy_proto::*;
    use ts_rs::TS;
    println!("// Generated from jimmy-proto.\nexport type Turn = number;");
    for declaration in [
        Mode::decl(),
        Codec::decl(),
        Configure::decl(),
        Word::decl(),
        Transcript::decl(),
        ClientEvent::decl(),
        EndReason::decl(),
        ServerEvent::decl(),
    ] {
        println!("export {declaration}");
    }
}

#[cfg(not(feature = "typescript"))]
fn main() {}
