# Submission posts

## X (required). Attach `demo/deja-demo.mp4`.

```
Project Name: Déjà
Team Members: @YOUR_X_HANDLE
Project Description: AI testing agents call the model on every run. Déjà lets Kimi K3.1 Pro on Cerebras test an app once through OpenClaw and Maestro. It checks the test, saves it, and replays it with 0 model calls. A changed button gets one small fix. A real bug still fails.
GitHub Repository: https://github.com/seankim-android/deja
Demo Video: (attached)
#KimiOnCerebras @Cerebras @Kimi_Moonshot
```

## Reddit, r/cerebras. Flair: "Kimi K3.1 Pro Hackathon". Attach the same video.

Title: Déjà: Kimi K3.1 Pro on Cerebras tests your app once. Every later run uses 0 model calls.

Body:

On the first run, Kimi K3.1 Pro on Cerebras drives an Android checkout through OpenClaw and Maestro. That took 13 model calls. Déjà then runs the new Maestro test again on its own. It saves the test only if all 14 checks pass.

Every later run is a cache hit: the same test, with 0 model calls.

We changed a button ID. Déjà fixed one selector and showed the diff. We also broke the app on purpose, and the test still failed. Déjà doesn't hide real bugs.

The saved test also replays in Chromium, with 0 model calls.

Repo: https://github.com/seankim-android/deja
