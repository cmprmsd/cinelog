# Cinelog
Comprehensive logging of all terminal input and output for each session based on Asciinema and wild zsh + Python scripting.

This tool is a strong modification of my former logging project https://github.com/cmprmsd/cLoggy with many features removed in order to focus on and extend the core features.

## Introduction

Each terminal in the context of root will create a `.cast` file and a `.commands.log` file. 

For each sudo user a directory will be created:
![Logging directory](docs/file-structure.png)

Each excecuted command will be logged and written to the commands.log file with the current timestamp of execution. All raw input and output will be streamed to the .cast file which contains escape sequences for every change in the terminal.

## Search
The main feature of this plugin is the log organization, search and web player.

The search functionality can be used with `hist` e.g. `hist <term>` and hist `<terms with spaces>`.

This will search the executed commands but not the output.
It will output a dropdown selection menu that you can navigate with keyboard or mouse(wheel). 
![Command search](docs/command-search.png)

In order to search through both the executed commands and the terminal output you may specify `-a` as first argument.

![Results for input and output](docs/input-output-search.png)

You'll differentiate commands from output by the filename of each match.

Once you press `enter` on a selection, the link to the `Cinelog Viewer` will be printed to the terminal.

## Cinelog Viewer
This feature is a simple webserver. Your selected cast will be viewed in this web player.

I applied some magic to summon the ingestion of `chapter markers` to the video timeline for each executed command. That way, you can easily navigate between commands via `[` and `]`.

## Extras

### Per Folder Logs
By default for each cwd a history file will be created in `~/.zsh_history.d/`. Using the alias `here` the executed commands in this directory will be printed, which is helpful for less often used git repositories you don't know how you got working.

### Message of the day
The terminal will by default output some information about the current networking stack, disk space and the log destination.
![terminal banner](docs/motd.png)

### Customization
You can opt-out of several extra features by changing your config file in `.config/cinelog/cinelog-settings.conf`.