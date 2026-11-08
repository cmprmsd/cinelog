//! `cinelog tls show|regen` over `cinelog_web::tls` (plan §7.1).

use std::process::ExitCode;

use cinelog_web::hosts::HostAllowlist;
use cinelog_web::tls::{self, CertInfo, TlsOptions};

use crate::cli::{TlsAction, TlsArgs};
use crate::ctx::Ctx;

pub fn run(args: &TlsArgs) -> ExitCode {
    let ctx = Ctx::load();
    let web = ctx.web_context();
    let opts = TlsOptions::from_context(&web, HostAllowlist::new(&web).cert_names());
    tls::install_provider();
    let result = match args.action {
        TlsAction::Show { json } => tls::inspect(&opts).map(|info| match info {
            Some(info) => {
                print!("{}", render(&info, json));
                ExitCode::SUCCESS
            }
            None => {
                eprintln!("cinelog: no certificate yet; the server creates one on its first start");
                ExitCode::FAILURE
            }
        }),
        TlsAction::Regen => tls::regenerate(&opts).map(|info| {
            print!("{}", render(&info, false));
            println!("restart the server to use it ({})", restart_hint(&ctx));
            ExitCode::SUCCESS
        }),
    };
    result.unwrap_or_else(|e| {
        eprintln!("cinelog: tls: {e}");
        ExitCode::from(2)
    })
}

fn render(info: &CertInfo, json: bool) -> String {
    if json {
        return format!("{}\n", serde_json::to_string(info).expect("cert info serializes"));
    }
    let expires = jiff::Timestamp::from_second(info.not_after)
        .map(|t| t.strftime("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| info.not_after.to_string());
    format!(
        "fingerprint {}\nself-signed {}\nexpires {expires}\nnames {}\ncert {}\n",
        info.fingerprint,
        if info.self_signed { "yes" } else { "no" },
        info.sans.join(", "),
        info.cert_path.display()
    )
}

fn restart_hint(ctx: &Ctx) -> &'static str {
    match ctx.scope() {
        cinelog_core::Scope::Team => "systemctl try-restart cinelog-team",
        cinelog_core::Scope::Personal => "systemctl --user try-restart cinelog",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn text_and_json() {
        let info = CertInfo {
            fingerprint: "AB:CD".into(),
            self_signed: true,
            not_after: 1_900_000_000,
            sans: vec!["box".into(), "127.0.0.1".into()],
            cert_path: PathBuf::from("/c/cert.pem"),
        };
        assert_eq!(
            render(&info, false),
            "fingerprint AB:CD\nself-signed yes\nexpires 2030-03-17\nnames box, 127.0.0.1\ncert /c/cert.pem\n"
        );
        let v: serde_json::Value = serde_json::from_str(&render(&info, true)).unwrap();
        assert_eq!(v["fingerprint"], "AB:CD");
        assert_eq!(v["not_after"], 1_900_000_000);
    }
}
