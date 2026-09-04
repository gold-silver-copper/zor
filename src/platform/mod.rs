pub type Pid = i32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Process {
    pub pid: Pid,
    pub ppid: Pid,
    pub comm: String,
    pub argv0: Option<String>,
    pub argv: Vec<String>,
    pub env_agent: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    pub leader: Pid,
    pub processes: Vec<Process>,
}
