// build.rs
fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Proto dosyasının varlığını kontrol et ve derle
    tonic_build::compile_protos("proto/observer.proto")?;
    Ok(())
}