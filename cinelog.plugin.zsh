#!/usr/bin/env bash

####################################################
# Asciinema Terminal Logging - Cinelog
# Author: Sebastian Haas - https://h8.to
####################################################

# Color definitions using 24-bit true color (RGB)
RESET='\e[0m'  # Resets all attributes
BOLD='\e[1m'

# Basic colors
BASIC_RED='\e[31m'
BASIC_GREEN='\e[32m'
BASIC_BLUE='\e[34m'
BAISC_MAGENTA='\e[35m'
BASIC_CYAN='\e[36m'
# Pastel Colors
PASTEL_RED='\e[38;2;255;105;97m'
PASTEL_GREEN='\e[38;2;119;221;119m'
PASTEL_YELLOW='\e[38;2;253;253;150m'
PASTEL_BLUE='\e[38;2;174;198;207m'
PASTEL_MAGENTA='\e[38;2;244;154;194m'
PASTEL_CYAN='\e[38;2;173;216;230m'

# Determine user home directory for configuration and logging
if [[ $EUID -eq 0 ]]; then
    # Running as root
    if [[ -n $SUDO_USER ]]; then
        # Use the home directory of the sudo user
        CONFIG_DIR=$(getent passwd "$SUDO_USER" | cut -d: -f6)/.config
		# Prepare logging
		LOGDIR="$HOME/logs/$SUDO_USER"
    else
        CONFIG_DIR="/root/.config"
		LOGDIR="$HOME/logs/root"
    fi
else
    # Running as a regular user
    CONFIG_DIR="$HOME/.config"
	LOGDIR="$HOME/logs"
fi
mkdir -p "$LOGDIR"

# Ensure CONFIG_DIR is set
if [[ -z $CONFIG_DIR ]]; then
    echo "Error: Unable to determine the configuration directory."
    exit 1
fi

# Define plugin relevant aliases and functions
# Per-folder history search (cd /root/randomGitFolderFromLastDecade/ && here)
here() {
    local history_file="$HOME/.zsh_history.d/$PWD/history"
    if [[ -f $history_file ]]; then
        sed 's/\x1b\[[0-9;]*m//g' "$history_file"  # Strip ANSI escape codes
    else
        echo "No history available for this directory."
    fi
}

# Clear screen with MOTD
alias cs="clear; load_motd"

# Interactive history search with graphical Asciinema UI
alias hist="$HOME/.zim/modules/cinelog/hist"
alias histo="$HOME/.zim/modules/cinelog/histo"

# MOTD functionality
load_motd() {
    echo -e "${PASTEL_GREEN}Logging to: file://${LOGFILE}${RESET}"
    if [[ $MOTD_ENABLED -eq 1 ]]; then
        echo -e "Uptime: $(uptime -p)"
        echo -e "Networks:\n$(ip -brief -color -4 a | grep -v UNKNOWN)"
        if [[ $GET_EXTERNAL_IP -eq 1 ]]; then
            if EXT_IP=$(curl --connect-timeout 1 -m 1 -4 https://my.ip.fi/ 2>/dev/null); then
                echo -e "${BASIC_CYAN}External IP:${RESET}\t ${BASIC_GREEN}UP\t\t${PASTEL_MAGENTA}${EXT_IP} ${RESET}GW:${PASTEL_MAGENTA} $(ip route | awk '/default/ {print $3; exit}')${RESET} ($(ip route | awk '/default/ {print $5; exit}'))"
            else
                echo -e "${BASIC_CYAN}External IP:${RESET}\t ${BASIC_RED}DOWN${RESET}\t        ${PASTEL_MAGENTA}Proxy or Offline${RESET}"
            fi
        else
            echo ""
        fi

        if [[ $CHECK_FREE_SPACE -eq 1 ]]; then
            timeout -k 2 2 df -H --output=pcent,target | tail -n +2 | while read -r output; do
                USE_PERCENT=$(echo "$output" | awk '{print $1}' | tr -d '%')
                PARTITION=$(echo "$output" | awk '{print $2}')
                if [[ $USE_PERCENT -ge 90 ]]; then
                    echo -e "${PASTEL_YELLOW}Running out of disk space ${BOLD}${PASTEL_YELLOW}\"${PARTITION} (${PASTEL_RED}${USE_PERCENT}%${PASTEL_YELLOW})\"${RESET}"
                fi
            done
        fi
    fi
}

# Create and read configuration file
cinelog_check_config() {
    local config_file="$CONFIG_DIR/cinelog/cinelog-settings.conf"
    if [[ ! -s $config_file ]]; then
        mkdir -p "$(dirname "$config_file")"
        cat > "$config_file" <<EOL
############## Config ###############
# Show MOTD
MOTD_ENABLED=1
# Show start and end time ([YYYY-MM-DD HH:MM:SS])
SHOW_START_END_TIME=1
# Per-folder Zsh history
USE_PER_FOLDER_HISTORY=1
# Check each mount point for free space with exceptions for tmpfs and cdroms
CHECK_FREE_SPACE=1
# Get external IP
GET_EXTERNAL_IP=0
EOL
		if [[ -n $SUDO_USER ]]; then
			chown -R "$SUDO_USER:$SUDO_GID" "$CONFIG_DIR/cinelog"
		fi
    fi
    # Source the configuration file
    # shellcheck disable=SC1090
    source "$config_file"
}

# Configuration file
cinelog_check_config

# Check if terminal is already logging; else start Asciinema
if [[ -n $ASCIINEMA_REC ]]; then
    load_motd
else
    # Check for webserver and spawn if not running
    if ! ss -tln | grep -q ":10000"; then
		{
        	nohup python -m http.server --directory "$HOME/.zim/modules/cinelog/asciinema-player" 10000 >/dev/null 2>&1 &
		} &>/dev/null
    fi
    export LOGFILE="$LOGDIR/terminal_$(date +%F_%T:%3N).cast"
    export COMMANDS_LOGFILE="${LOGFILE}.commands.log"
    touch "$COMMANDS_LOGFILE"  # Create command logfile
    asciinema rec -q "$LOGFILE"
    exit
fi

# Function to log commands before execution
preexec() {
    local cmd="$3"
    # Per-folder history
    if [[ $USE_PER_FOLDER_HISTORY -eq 1 ]]; then
        if [[ ! $cmd =~ ^here.* && $cmd != "zsh" ]]; then
            mkdir -p "$HOME/.zsh_history.d/$PWD/"
			touch "$HOME/.zsh_history.d/$PWD/history"
            echo "$cmd" >> "$HOME/.zsh_history.d/$PWD/history"
        fi
    fi
    # Add Cinelog marker for executed command
    local command_log_time
    command_log_time=$(tail -n1 "$LOGFILE" | awk -F, '{print substr($1, 2)}' | awk '{printf "%f", $1 - 0.1}')
    # Exclude /hist command
    if [[ $cmd != */hist* ]]; then
        # First, replace newlines with semicolons
		processed_cmd="${cmd//$'\n'/;}"

		# Then, escape double quotes
		processed_cmd="${processed_cmd//\"/\\\"}"

		# Now, use the processed_cmd variable in your echo statement
		echo "[${command_log_time}, \"m\", \"${processed_cmd}\"]" >> "$COMMANDS_LOGFILE"
    fi
    # Format and echo begin time of command execution
    if [[ $SHOW_START_END_TIME -eq 1 ]]; then
        local date_str
        date_str="Begin [$(date '+%Y-%m-%d %H:%M:%S')]"
        printf '%*s\n' "$COLUMNS" "$date_str"
    fi
    # Show current process in window name
    print -Pn '\e]0;%n@%m: %~ '"${cmd}"'\a'
}

# Function to execute before the next prompt
endTime() {
    # Format and echo end time of command execution
    if [[ $SHOW_START_END_TIME -eq 1 ]]; then
        local date_str
        date_str="End   [$(date '+%Y-%m-%d %H:%M:%S')]"
        printf '%*s\n' "$COLUMNS" "$date_str"
    fi
}

precmd_functions+=(endTime)
