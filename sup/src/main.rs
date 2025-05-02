use anyhow::{anyhow, Result};
use clap::Parser;
use log::{error, info};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task;
use tokio::time::sleep;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the script to run
    #[arg(short, long, default_value = "/home/ghuntley/code/amp.sh")]
    script: String,

    /// Number of tmux panes to create
    #[arg(short, long, default_value_t = 4)]
    panes: usize,

    /// Base path for log files
    #[arg(short, long, default_value = "/tmp/amp")]
    log_base: String,
}

struct TmuxPane {
    window_index: usize,
    pane_index: usize,
    log_file: PathBuf,
    script: String,
    working_dir: String,
    last_activity: std::time::Instant,
}

impl TmuxPane {
    fn new(index: usize, script: String, log_base: &str, working_dir: String) -> Self {
        let log_file = PathBuf::from(format!("{}-{}.log", log_base, index));
        Self {
            window_index: 0, // Assume we're using the first window
            pane_index: index,
            log_file,
            script,
            working_dir,
            last_activity: std::time::Instant::now(),
        }
    }
    
    fn update_activity(&mut self) {
        self.last_activity = std::time::Instant::now();
    }

    fn restart_command(&self) -> Result<()> {
        info!("Restarting command in pane {}", self.pane_index);

        // Send C-c to kill any existing process
        self.send_keys("C-c")?;
        std::thread::sleep(Duration::from_millis(500)); // Small delay

        // Change directory first
        let cd_command = format!("cd {}", self.working_dir);
        self.send_keys(&cd_command)?;
        self.send_keys("Enter")?;
        std::thread::sleep(Duration::from_millis(200)); // Small delay

        // Run the command with the environment variables set
        let env_vars = format!("export AMP_LOG_FILE={} && export AMP_LOG_LEVEL=debug", self.log_file.display());
        let command = format!("{} && {}", env_vars, self.script);
        self.send_keys(&command)?;
        self.send_keys("Enter")?;

        Ok(())
    }

    fn send_keys(&self, keys: &str) -> Result<()> {
        let status = Command::new("tmux")
            .args([
                "send-keys",
                "-t",
                &format!("{}.{}", self.window_index, self.pane_index),
                keys,
            ])
            .status()?;

        if !status.success() {
            return Err(anyhow!("Failed to send keys to tmux pane"));
        }

        Ok(())
    }
}

fn setup_tmux(panes: usize) -> Result<()> {
    // Start a new tmux session if not already in one
    if std::env::var("TMUX").is_err() {
        let status = Command::new("tmux")
            .args(["new-session", "-d"])
            .status()?;

        if !status.success() {
            return Err(anyhow!("Failed to create tmux session"));
        }
    }

    // Split the window into the specified number of panes
    for i in 1..panes {
        let status = Command::new("tmux")
            .args(["split-window", "-h"])
            .status()?;

        if !status.success() {
            return Err(anyhow!("Failed to split tmux window"));
        }

        // After each split, select the first pane to split it again
        if i < panes - 1 {
            let status = Command::new("tmux")
                .args(["select-pane", "-t", "0"])
                .status()?;

            if !status.success() {
                return Err(anyhow!("Failed to select tmux pane"));
            }
        }
    }

    // Select layout for even distribution
    let status = Command::new("tmux")
        .args(["select-layout", "even-horizontal"])
        .status()?;

    if !status.success() {
        return Err(anyhow!("Failed to set tmux layout"));
    }

    Ok(())
}

async fn tail_log(pane: Arc<Mutex<TmuxPane>>) -> Result<()> {
    let locked_pane = pane.lock().await;
    let log_path = locked_pane.log_file.clone();
    drop(locked_pane); // Release the lock

    let pane_clone = Arc::clone(&pane);

    // Create empty log file if it doesn't exist
    if !log_path.exists() {
        File::create(&log_path)?;
    }

    task::spawn(async move {
        let mut last_pos = 0;
        let mut had_activity = false;
        
        loop {
            // Check if there's been inactivity for more than 2 minutes
            let mut locked_pane = pane_clone.lock().await;
            let inactivity_duration = locked_pane.last_activity.elapsed();
            
            // If process was active before and now has been inactive for 2+ minutes, restart it
            if had_activity && inactivity_duration > Duration::from_secs(120) {
                info!("No activity detected for {} minutes in pane {}. Restarting process...", 
                      inactivity_duration.as_secs() / 60, locked_pane.pane_index);
                
                if let Err(e) = locked_pane.restart_command() {
                    error!("Failed to restart inactive command: {}", e);
                }
                
                // Reset activity tracking
                locked_pane.update_activity();
            }
            drop(locked_pane);
            
            // Open the file for reading
            let mut file = match OpenOptions::new().read(true).open(&log_path) {
                Ok(file) => file,
                Err(e) => {
                    error!("Error opening log file: {}", e);
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            let file_size = match file.metadata() {
                Ok(metadata) => metadata.len(),
                Err(e) => {
                    error!("Error getting file metadata: {}", e);
                    sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };

            // If file was truncated, reset position
            if file_size < last_pos {
                last_pos = 0;
            }

            // Seek to last read position
            if let Err(e) = file.seek(SeekFrom::Start(last_pos)) {
                error!("Error seeking in log file: {}", e);
                sleep(Duration::from_secs(1)).await;
                continue;
            }

            let reader = BufReader::new(file);
            let mut new_content = false;
            
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        // Mark that we've had activity
                        had_activity = true;
                        new_content = true;
                        
                        // Check for specific error patterns
                        if line.contains("prompt is too long") || 
                           line.contains("after property name in JSON at") || 
                           line.contains("exceed context limit:") ||
                           line.contains("Shutting down...") {
                            info!("Error detected in log: {}", line);

                            // Restart the command
                            let mut locked_pane = pane_clone.lock().await;
                            if let Err(e) = locked_pane.restart_command() {
                                error!("Failed to restart command: {}", e);
                            }
                            // Update activity timestamp since we just acted on this process
                            locked_pane.update_activity();
                            drop(locked_pane);
                        }
                        
                        // Update the last position
                        last_pos += line.len() as u64 + 1; // +1 for newline
                    }
                    Err(e) => {
                        error!("Error reading line: {}", e);
                        break;
                    }
                }
            }
            
            // Update activity timestamp if we saw new content
            if new_content {
                let mut locked_pane = pane_clone.lock().await;
                locked_pane.update_activity();
                drop(locked_pane);
            }

            // Wait before checking for new content
            sleep(Duration::from_millis(500)).await;
        }
    });

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logger
    simple_logger::init_with_level(log::Level::Info)?;

    // Parse command line arguments
    let args = Args::parse();

    // Setup tmux with the specified number of panes
    setup_tmux(args.panes)?;

    // Define working directories for each pane
    let working_dirs = [
        "/home/ghuntley/code/anole",
        "/home/ghuntley/code/it",
        "/home/ghuntley/code/pherrit",
        "/home/ghuntley/code/squirrel",
    ];

    // Create and configure the panes
    let mut panes = Vec::new();
    for i in 0..args.panes {
        let working_dir = if i < working_dirs.len() {
            working_dirs[i].to_string()
        } else {
            // Fallback if more panes than working dirs
            "/home/ghuntley/code".to_string()
        };

        let pane = TmuxPane::new(i, args.script.clone(), &args.log_base, working_dir);
        panes.push(Arc::new(Mutex::new(pane)));
    }

    // Start the commands in each pane
    for pane in &panes {
        let locked_pane = pane.lock().await;
        if let Err(e) = locked_pane.restart_command() {
            error!("Failed to start command in pane {}: {}", locked_pane.pane_index, e);
        }
    }

    // Start tailing the logs
    let mut tasks = Vec::new();
    for pane in panes {
        let task = tail_log(pane);
        tasks.push(task);
    }

    // Wait for all tasks
    for task in tasks {
        if let Err(e) = task.await {
            error!("Task error: {}", e);
        }
    }

    // Keep the main thread alive
    loop {
        sleep(Duration::from_secs(60)).await;
    }
}