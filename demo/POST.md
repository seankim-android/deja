# Submission posts

## X (required). Attach `demo/deja-demo.mp4`. Post when you can stay online for 30 minutes.

```
My AI testing agent learned our checkout in 13 model calls. Now it replays it with 0.

Project Name: Déjà (formerly WalkCache)
Team Members: @YOUR_X_HANDLE
Project Description: Kimi K3.1 Pro on Cerebras explores an Android app once through OpenClaw and writes a Maestro test. Déjà runs that test again by itself and saves it only if all 14 checks pass. Every later run replays the saved test with no model. A real bug still fails the test.
GitHub Repository: https://github.com/seankim-android/deja
Demo Video: (attached)
#KimiOnCerebras @Cerebras @Kimi_Moonshot
```

Other first lines (swap in for line 1):
- B: "Paying a model to tap the same Sign In button every night felt wrong. So I made the agent remember."
- C: "Kimi on Cerebras tests my app once. After that, the test runs with no model at all."

### First reply (post it right after, under your own post)

```
The part I care about most is what Déjà refuses to fix.

We renamed a button ID. Déjà changed one selector, showed the diff and made no commit. Then we broke the order total on purpose. The test failed, and Déjà made 0 repair attempts.

Every number in the video comes from a saved run report in the repo.
```

## Reddit, r/cerebras. Flair: "Kimi K3.1 Pro Hackathon". Attach the same video.

Title: I made Kimi K3.1 Pro on Cerebras test my app once, then replay the test with 0 model calls

Body:

AI testing agents are good at finding their way through an app. But most of them call the model again on every run, even when the app hasn't changed. I wanted the agent to learn a flow once and then stay out of the way.

Déjà (formerly WalkCache) works in three steps:

1. Kimi K3.1 Pro on Cerebras drives an Android checkout through OpenClaw and Maestro. In our run, that took 13 model calls.
2. Déjà runs the Maestro test it wrote again, with no model. It saves the test only if all 14 checks pass.
3. Every later run is a cache hit: the same YAML, 0 model calls.

Two things I tested on purpose. When we renamed a button ID, Déjà made one small fix, showed the diff and didn't commit it. When we broke the order total, the test failed and Déjà didn't try to repair it. A test that "fixes" real bugs is worse than no test.

What it doesn't do yet: cold exploration and repair are Android only. Chromium and iOS can replay a saved test but can't learn one. Replay also doesn't make Maestro faster. It removes the model calls.

Every number in the video comes from a run report in the repo: https://github.com/seankim-android/deja

I'd like to know which flow you would cache first: sign-in, checkout or onboarding?

## Engagement plan (first hour)
1. Post on X. Then post the first reply within 1 minute.
2. Answer every reply on X for 30 minutes, with a specific fact each time.
3. Post on Reddit about 60 minutes after X, not at the same moment.
4. Quote-post your own X post once, with the Reddit link or a new clip, the next morning before the Tue 10 AM PT count.
