// Git hooks export repository-local variables, including alternate commit indexes.
// Test repositories must never inherit that routing or personal Git configuration.
export function gitFixtureEnv(home) {
    return {
        PATH: process.env.PATH,
        HOME: home,
        XDG_CONFIG_HOME: `${home}/.config`,
        GIT_CONFIG_GLOBAL: "/dev/null",
        GIT_CONFIG_NOSYSTEM: "1",
        GIT_TERMINAL_PROMPT: "0",
    };
}
