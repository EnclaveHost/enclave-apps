//! The build embeds the static embedding model (src/embed.rs); say so
//! plainly when it is missing instead of failing inside include_bytes!.
fn main() {
    println!("cargo:rerun-if-changed=model/embeddings.i8");
    println!("cargo:rerun-if-changed=model/vocab.txt");
    for f in ["model/embeddings.i8", "model/vocab.txt"] {
        if !std::path::Path::new(f).exists() {
            panic!("{f} is missing: run scripts/fetch-model.sh first (it downloads and converts minishlab/potion-base-8M, pinned by digest)");
        }
    }
}
