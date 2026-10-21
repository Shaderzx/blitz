fn main() {
    // One line per input the tests need to see: the library value, the gitignored data file that
    // `include_str!` compiles in, and the listing the build script read from the watched directory.
    // A stale whole-build hit restores an old binary, so these lines are how the tests notice it.
    println!("{}", fixture_lib::value());
    println!("data:{}", include_str!("../../data/value.txt").trim());
    println!("watched:{}", include_str!(concat!(env!("OUT_DIR"), "/watched.txt")));
}
