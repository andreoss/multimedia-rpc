use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Server,
    Worker,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Role::Server => "server",
            Role::Worker => "worker",
        }
    }
}

fn parse_role(args: &[String]) -> Result<Role, String> {
    match args {
        [role] if role == "server" => Ok(Role::Server),
        [role] if role == "worker" => Ok(Role::Worker),
        _ => Err(String::from("usage: mmrpcd <server|worker>")),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_role(&args) {
        Ok(role) => {
            println!("{}", role.name());
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Role, parse_role};

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn server_role_parses() {
        assert_eq!(parse_role(&args(&["server"])), Ok(Role::Server));
    }

    #[test]
    fn worker_role_parses() {
        assert_eq!(parse_role(&args(&["worker"])), Ok(Role::Worker));
    }

    #[test]
    fn unknown_role_rejected() {
        assert!(parse_role(&args(&["observer"])).is_err());
    }

    #[test]
    fn missing_role_rejected() {
        assert!(parse_role(&args(&[])).is_err());
    }

    #[test]
    fn extra_arguments_rejected() {
        assert!(parse_role(&args(&["server", "worker"])).is_err());
    }

    #[test]
    fn role_names_render() {
        assert_eq!(Role::Server.name(), "server");
        assert_eq!(Role::Worker.name(), "worker");
    }
}
