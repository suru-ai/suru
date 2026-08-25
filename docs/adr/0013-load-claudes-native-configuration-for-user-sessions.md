# Load Claude's native configuration for user Sessions

Claude Sessions and Skill catalog discovery load the user's personal and project setting sources so Claude's native Skills, overrides, hooks, and MCP servers behave as configured; availability probes, Model discovery, and Errands remain isolated. Claude offers no selective way to load Skills without those other settings, and reimplementing its Skill discovery would diverge from the Provider's own precedence and visibility rules.
