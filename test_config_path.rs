// Test script to verify config path behavior
use std::env;

fn get_config_path() -> Result<String, Box<dyn std::error::Error>> {
    #[cfg(debug_assertions)]
    {
        // In debug mode, look for config.toml in the current directory
        Ok("config.toml".to_string())
    }
    
    #[cfg(not(debug_assertions))]
    {
        // In release mode, look for config.toml in $HOME/.config/hars-imp
        let home = env::var("HOME")
            .map_err(|_| "HOME environment variable not set")?;
        let config_path = format!("{}/.config/hars-imp/config.toml", home);
        
        Ok(config_path)
    }
}

fn main() {
    println!("Config path: {}", get_config_path().unwrap());
    println!("Debug assertions enabled: {}", cfg!(debug_assertions));
}
