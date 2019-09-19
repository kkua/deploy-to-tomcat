#[macro_use]
extern crate serde_derive;

use pbr::{ProgressBar, Units};
use ssh2::Session;
use std::collections::hash_map::HashMap;
use std::io::prelude::*;
use std::net::TcpStream;
use std::path::Path;

#[derive(Deserialize, Debug)]
struct DeployConfig {
    #[serde(default)]
    server: ServerConfig,
    app_name: String,
}

#[derive(Deserialize, Debug)]
#[serde(default)]
struct ServerConfig {
    addr: String,
    username: String,
    password: String,
    tomcat_path: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            addr: "192.168.1.2".to_owned(),
            username: "username".to_owned(),
            password: "password".to_owned(),
            tomcat_path: "/usr/local/tomcat".to_owned(),
        }
    }
}

#[derive(PartialEq, Debug)]
struct Record {
    sign: String,
    len: u64,
    delete: bool,
}

fn load_config() -> DeployConfig {
    let conf_path = "config.toml";
    let conf_content = std::fs::read(&conf_path).expect("Failed to read config file `config.toml`");
    return toml::from_slice(&conf_content).expect("Failed to parse config.toml");
}

static RECORD_DIR_PATH: &str = "~/deploy-record";

fn main() {
    let config = load_config();
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        println!("缺少上传目录路径参数");
        std::process::exit(1);
    } else {
        let dir_path = Path::new(&args[1]);
        if dir_path.exists() && dir_path.is_dir() {
            let tomcat_app_dir_path = config.server.tomcat_path.to_owned() + "/webapps/";
            let deploy_dir_path = tomcat_app_dir_path + "/" + &config.app_name;
            let record_file_path = RECORD_DIR_PATH.to_owned() + "/" + &config.app_name + ".rec";
            let tcp = TcpStream::connect(&config.server.addr).unwrap();
            let sess = connect_ssh(&tcp, &config.server.username, &config.server.password);
            let mut records = parse_record_file(&sess, &record_file_path, &deploy_dir_path);
            upload_dir(
                &sess,
                dir_path,
                &deploy_dir_path,
                &mut records,
                &record_file_path,
            );
            println!("操作完成");
        } else {
            println!("路径：{} 不存在或者不是一个目录", args[1]);
            std::process::exit(1);
        }
    }
}

fn parse_record_file(
    sess: &Session,
    record_file_path: &str,
    deploy_dir_path: &str,
) -> HashMap<String, Record> {
    let sftp = sess.sftp().unwrap();
    let mut record_file_stream = sftp.open(Path::new(record_file_path));
    if !record_file_stream.is_ok() {
        exec_command(
            &sess,
            &format!("mkdir -p {} && touch {}", RECORD_DIR_PATH, record_file_path),
        );
        record_file_stream = sftp.open(Path::new(record_file_path));
    }
    let mut record_file = record_file_stream.unwrap();
    let mut record_contents = String::new();

    record_file.read_to_string(&mut record_contents).unwrap();
    let mut records = HashMap::new();
    if !record_contents.is_empty() {
        let content_line_list = record_contents.split_terminator('\n');
        for content_line in content_line_list {
            let field_list: Vec<_> = content_line.split(" , ").collect();
            let sign = String::from(field_list[0]);
            let path = String::from(field_list[1]);
            let len = u64::from_str_radix(&field_list[2], 10u32).unwrap();
            let record: Record = Record {
                sign: sign,
                len: len,
                delete: true,
            };
            records.insert(path, record);
        }
    } else {
        // 删除所有旧文件
        exec_command(&sess, &format!("rm -rf {}/*", deploy_dir_path));
    }
    records
}

fn transport_file(
    sess: &Session,
    to_be_upload_path: &Path,
    dest_path: &str,
    file_content: Vec<u8>,
) {
    let file_size = file_content.len();
    let mut pb = ProgressBar::new(file_size as u64);
    pb.set_units(Units::Bytes);
    println!("{}  {}", "上传", to_be_upload_path.to_str().unwrap());
    let mut remote_file = sess
        .scp_send(Path::new(dest_path), 0o644, file_size as u64, None)
        .unwrap();
    let mut wrote_size = 0usize;
    while wrote_size < file_size {
        let incr_size = remote_file.write(&file_content[wrote_size..]).unwrap();
        wrote_size = wrote_size + incr_size;
        pb.add(incr_size as u64);
    }
    pb.finish();
    println!();
}

fn walk_dir(
    sess: &Session,
    dir_path: &Path,
    to_dir: &str,
    releative_path: &str,
    records: &mut HashMap<String, Record>,
) {
    for entry in std::fs::read_dir(dir_path).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if !path.is_dir() {
            let to_be_upload_path = dir_path.join(entry.file_name().to_str().unwrap());
            let mut to_be_upload_file_content: Vec<u8> = Vec::new();
            let mut to_be_upload_file = std::fs::File::open(Path::new(&to_be_upload_path)).unwrap();
            let file_size = to_be_upload_file
                .read_to_end(&mut to_be_upload_file_content)
                .unwrap() as u64;
            let file_md5 = format!("{:x}", md5::compute(&to_be_upload_file_content));
            let file_releative_path =
                format!("{}/{}", releative_path, entry.file_name().to_str().unwrap());
            if records.contains_key(&file_releative_path) {
                let record = records.get_mut(&file_releative_path).unwrap();
                record.delete = false;
                if record.len == file_size && record.sign == file_md5 {

                } else {
                    // 文件变化
                    record.len = file_size;
                    record.sign = file_md5;
                    transport_file(
                        sess,
                        &to_be_upload_path,
                        &(to_dir.to_owned() + "/" + entry.file_name().to_str().unwrap()),
                        to_be_upload_file_content,
                    );
                }
            } else {
                let record = Record {
                    sign: String::from(file_md5),
                    len: file_size,
                    delete: false,
                };
                records.insert(file_releative_path, record);
                transport_file(
                    sess,
                    &to_be_upload_path,
                    &(to_dir.to_owned() + "/" + entry.file_name().to_str().unwrap()),
                    to_be_upload_file_content,
                );
            }
        } else {
            let releative_path =
                &(releative_path.to_owned() + "/" + entry.file_name().to_str().unwrap());
            let to_dir = &(to_dir.to_owned() + "/" + entry.file_name().to_str().unwrap());
            exec_command(sess, &("mkdir -p ".to_owned() + to_dir));
            walk_dir(sess, path.as_path(), to_dir, releative_path, records);
        }
    }
}

fn upload_dir(
    sess: &Session,
    dir_path: &Path,
    to_dir: &str,
    records: &mut HashMap<String, Record>,
    record_file_path: &str,
) {
    walk_dir(sess, dir_path, to_dir, &"", records);
    records.retain(|path, ref mut record| {
        if record.delete {
            println!("删除 {}", path);
            exec_command(sess, &("rm -f ".to_owned() + to_dir + "/" + path));
            false
        } else {
            true
        }
    });
    let mut records_str = String::new();
    for (path, record) in records {
        records_str.push_str(&format!("{} , {} , {}\n", record.sign, path, record.len));
    }
    let file_content = records_str.as_bytes();
    let file_size = file_content.len();
    let mut remote_file = sess
        .scp_send(Path::new(record_file_path), 0o644, file_size as u64, None)
        .unwrap();
    let mut wrote_size = 0usize;
    while wrote_size < file_size {
        let incr_size = remote_file.write(&file_content[wrote_size..]).unwrap();
        wrote_size = wrote_size + incr_size;
    }
}

fn connect_ssh(tcp: &TcpStream, user_name: &str, password: &str) -> Session {
    let mut sess = Session::new().unwrap();
    sess.handshake(&tcp).unwrap();
    sess.userauth_password(user_name, password).unwrap();
    return sess;
}

fn exec_command(sess: &Session, command: &str) -> String {
    let mut channel = sess.channel_session().unwrap();
    channel.exec(command).unwrap();
    let mut s = String::new();
    channel.read_to_string(&mut s).unwrap();
    channel.wait_close().unwrap();
    return s;
}
