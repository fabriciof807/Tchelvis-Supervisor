import { test } from "node:test";
import assert from "node:assert/strict";
import {
  ICONS,
  drawerHeaderTitle,
  doctorUpdateCardState,
  tailErrorText,
  parseUpdateNewVersion,
} from "../../assets/herdr-app/mods/tchelvis/main.js";

test("drawerHeaderTitle returns icon, title, and subtitle for every view", () => {
  const insWithAgent = drawerHeaderTitle("inspector", {
    agent: { name: "cockpit", address: "t1b3" },
  });
  assert.deepEqual(insWithAgent, {
    icon: "inspector",
    title: "Inspector",
    subtitle: "cockpit · t1b3",
  });
  assert.ok(Object.hasOwn(ICONS, insWithAgent.icon));

  const insNoAgent = drawerHeaderTitle("inspector", {});
  assert.deepEqual(insNoAgent, {
    icon: "inspector",
    title: "Inspector",
    subtitle: null,
  });

  const home = drawerHeaderTitle("home", {});
  assert.deepEqual(home, {
    icon: "home",
    title: "Home",
    subtitle: null,
  });
  assert.ok(Object.hasOwn(ICONS, home.icon));

  const doctor = drawerHeaderTitle("doctor", {
    updateCheck: { current_version: "0.2.0", current_commit: "dfcf395" },
  });
  assert.deepEqual(doctor, {
    icon: "doctor",
    title: "Doctor",
    subtitle: "v0.2.0 (dfcf395)",
  });
  assert.ok(Object.hasOwn(ICONS, doctor.icon));

  const doctorFallback = drawerHeaderTitle("doctor", {
    doctorReport: { cockpit_version: "0.2.0" },
  });
  assert.deepEqual(doctorFallback, {
    icon: "doctor",
    title: "Doctor",
    subtitle: "v0.2.0",
  });

  const chat = drawerHeaderTitle("chat", {});
  assert.deepEqual(chat, {
    icon: "chat",
    title: "Chat",
    subtitle: null,
  });
  assert.ok(Object.hasOwn(ICONS, chat.icon));

  const usage = drawerHeaderTitle("inspector", { usageOnlyDrawer: true });
  assert.deepEqual(usage, {
    icon: "llm",
    title: "AI Usage",
    subtitle: null,
  });
  assert.ok(Object.hasOwn(ICONS, usage.icon));

  const modelPicker = drawerHeaderTitle("inspector", {
    modelPickerOnlyDrawer: true,
  });
  assert.deepEqual(modelPicker, {
    icon: "llm",
    title: "Supervisor model",
    subtitle: null,
  });
  assert.ok(Object.hasOwn(ICONS, modelPicker.icon));
});

test("doctorUpdateCardState maps saved channel with update available", () => {
  const card = doctorUpdateCardState({
    check: {
      current_version: "0.2.0",
      current_commit: null,
      channel: "stable",
      channel_source: "saved",
      latest_version: "0.3.0",
      latest_commit: null,
      update_available: true,
    },
  });

  assert.equal(card.currentLabel, "v0.2.0");
  assert.equal(card.channelLabel, "stable");
  assert.equal(card.latestLabel, "v0.3.0");
  assert.equal(card.badgeText, "DISPONÍVEL");
  assert.equal(card.buttons.length, 1);
  assert.deepEqual(card.buttons[0], {
    id: "ck-doc-update-run",
    label: "Atualizar (stable)",
    channel: "stable",
    action: "update",
    disabled: false,
    accent: true,
  });
});

test("doctorUpdateCardState maps saved channel when up to date", () => {
  const card = doctorUpdateCardState({
    check: {
      current_version: "0.2.0",
      current_commit: "a1b2c3d",
      channel: "nightly",
      channel_source: "saved",
      latest_version: "nightly",
      latest_commit: "a1b2c3d",
      update_available: false,
    },
  });

  assert.equal(card.currentLabel, "v0.2.0 (a1b2c3d)");
  assert.equal(card.channelLabel, "nightly");
  assert.equal(card.latestLabel, "nightly (a1b2c3d)");
  assert.equal(card.badgeText, "ATUALIZADO");
  assert.equal(card.buttons.length, 1);
  assert.deepEqual(card.buttons[0], {
    id: "ck-doc-update-run",
    label: "Atualizado",
    channel: "nightly",
    action: "none",
    disabled: true,
    accent: false,
  });
});

test("doctorUpdateCardState maps no saved channel to Stable and Nightly buttons", () => {
  const card = doctorUpdateCardState({
    check: {
      current_version: "0.2.0",
      current_commit: null,
      channel: null,
      channel_source: "none",
      latest_version: "0.2.0",
      latest_commit: null,
      update_available: true,
    },
  });

  assert.equal(card.channelLabel, "não definido");
  assert.equal(card.badgeText, "ESCOLHA O CANAL");
  assert.deepEqual(card.buttons, [
    {
      id: "ck-doc-update-stable",
      label: "Stable",
      channel: "stable",
      action: "update",
      disabled: false,
      accent: true,
    },
    {
      id: "ck-doc-update-nightly",
      label: "Nightly",
      channel: "nightly",
      action: "update",
      disabled: false,
      accent: false,
    },
  ]);
});

test("doctorUpdateCardState shows elapsed seconds and disables clicking while running", () => {
  const card = doctorUpdateCardState({
    check: {
      current_version: "0.2.0",
      current_commit: null,
      channel: "stable",
      channel_source: "saved",
      latest_version: "0.3.0",
      latest_commit: null,
      update_available: true,
    },
    running: true,
    startedAtMs: 1000,
    nowMs: 13400,
  });

  assert.equal(card.badgeText, "ATUALIZANDO");
  assert.equal(card.buttons.length, 1);
  assert.equal(card.buttons[0].label, "Atualizando… 12s");
  assert.equal(card.buttons[0].disabled, true);
  assert.equal(card.buttons[0].action, "none");
});

test("doctorUpdateCardState exposes error tail and retry/update buttons", () => {
  const installErr = doctorUpdateCardState({
    check: {
      current_version: "0.2.0",
      current_commit: null,
      channel: "stable",
      channel_source: "saved",
      latest_version: "0.3.0",
      latest_commit: null,
      update_available: true,
    },
    error: "permission denied replacing /usr/local/bin/tchelvis",
  });
  assert.equal(installErr.badgeText, "ERRO");
  assert.equal(
    installErr.errorText,
    "permission denied replacing /usr/local/bin/tchelvis"
  );
  assert.equal(installErr.buttons[0].disabled, false);

  const checkErr = doctorUpdateCardState({
    check: null,
    checking: false,
    checkError: "HTTP request failed: network unreachable",
  });
  assert.equal(checkErr.badgeText, "ERRO");
  assert.equal(checkErr.errorText, "HTTP request failed: network unreachable");
  assert.equal(checkErr.buttons[0].action, "check");
});

test("doctorUpdateCardState shows new version and restart note on success", () => {
  const card = doctorUpdateCardState({
    check: {
      current_version: "0.3.0",
      current_commit: null,
      channel: "stable",
      channel_source: "saved",
      latest_version: "0.3.0",
      latest_commit: null,
      update_available: false,
    },
    result: { newVersion: "0.3.0 (stable)", channel: "stable" },
  });

  assert.equal(card.successVersion, "0.3.0 (stable)");
  assert.equal(
    card.successNote,
    "Reinicie o Herdr Desktop para carregar o mod novo"
  );
});

test("tailErrorText and parseUpdateNewVersion handle real CLI output", () => {
  assert.equal(
    parseUpdateNewVersion(
      "Updated tchelvis: 0.2.0 (dev) → 0.3.0 (stable) (channel: stable)\nRestart Herdr Desktop to reload the mod.\n"
    ),
    "0.3.0 (stable)"
  );
  assert.equal(tailErrorText("", "", 2), "Falha ao atualizar (código 2)");
  assert.equal(
    tailErrorText("line1\nline2\nerror: boom\n", "", 1),
    "line1\nline2\nerror: boom"
  );
});
