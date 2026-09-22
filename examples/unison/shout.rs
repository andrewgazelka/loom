// Added with `--dep greet=greet`: the alias becomes the crate name below.
pub fn shout(name: String) -> String {
    greet::greet(name).to_uppercase()
}
