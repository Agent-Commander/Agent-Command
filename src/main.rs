fn main() -> std::io::Result<()> {
    agent_command::run_cli(&std::env::args().skip(1).collect::<Vec<_>>())
}
