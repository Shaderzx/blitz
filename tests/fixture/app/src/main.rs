fn main() {
    // A file the ignore rules hide, read at compile time: editing it must invalidate the build.
    let _data = include_str!("../../data/value.txt");
    println!("{}", fixture_lib::value());
}
