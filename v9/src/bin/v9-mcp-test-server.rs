use serde_json::{Value, json};
use std::{
    io::{self, BufRead, Write},
    thread,
    time::Duration,
};

fn read(reader: &mut impl BufRead) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn send(writer: &mut impl Write, value: Value) {
    serde_json::to_writer(&mut *writer, &value).unwrap();
    writeln!(writer).unwrap();
    writer.flush().unwrap();
}

fn main() {
    let scenario = std::env::args().nth(1).unwrap_or_else(|| "success".into());
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    let initialize = read(&mut reader);
    let id = initialize["id"].clone();
    send(
        &mut writer,
        json!({"jsonrpc":"2.0","id":id,"result":{
            "protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},
            "serverInfo":{"name":"mock","version":"1"}
        }}),
    );
    let initialized = read(&mut reader);
    assert_eq!(initialized["method"], "notifications/initialized");

    let first_list = read(&mut reader);
    let id = first_list["id"].clone();
    send(
        &mut writer,
        json!({"jsonrpc":"2.0","id":id,"result":{
            "tools":[{"name":"first","description":"first page","inputSchema":{"type":"object"}}],
            "nextCursor":"page-2"
        }}),
    );
    let second_list = read(&mut reader);
    assert_eq!(second_list["params"]["cursor"], "page-2");
    let id = second_list["id"].clone();
    let second_name = if scenario == "duplicate" {
        "first"
    } else {
        "echo"
    };
    send(
        &mut writer,
        json!({"jsonrpc":"2.0","id":id,"result":{
            "tools":[{"name":second_name,"description":"echo input","inputSchema":{"type":"object","additionalProperties":true}}]
        }}),
    );

    let call = read(&mut reader);
    let id = call["id"].clone();
    match scenario.as_str() {
        "success" => {
            send(
                &mut writer,
                json!({"jsonrpc":"2.0","id":77,"method":"sampling/createMessage","params":{}}),
            );
            let denied = read(&mut reader);
            assert_eq!(denied["error"]["code"], -32601);
            send(
                &mut writer,
                json!({"jsonrpc":"2.0","id":id,"result":{
                    "content":[{"type":"text","text":"hello"}],
                    "structuredContent":{"answer":42},"isError":false
                }}),
            );
        }
        "is-error" => send(
            &mut writer,
            json!({"jsonrpc":"2.0","id":id,"result":{
                "content":[{"type":"text","text":"remote failure"}],"isError":true
            }}),
        ),
        "timeout" => thread::sleep(Duration::from_secs(2)),
        "malformed" => {
            writeln!(writer, "not json").unwrap();
            writer.flush().unwrap();
        }
        "oversize" => {
            writeln!(writer, "{}", "x".repeat(8192)).unwrap();
            writer.flush().unwrap();
        }
        _ => panic!("unknown scenario"),
    }
}
