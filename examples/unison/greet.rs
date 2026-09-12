fn greeting() -> String {
    "hello, ".to_owned()
}

pub fn greet(name: String) -> String {
    let message = greeting() + &name;
    message
}
