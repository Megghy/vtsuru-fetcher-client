import { invoke } from "@tauri-apps/api/core";

let greetInputEl: HTMLInputElement | null;
let greetMsgEl: HTMLElement | null;

async function greet() {
  if (greetMsgEl && greetInputEl) {
    // Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
    greetMsgEl.textContent = await invoke("greet", {
      name: greetInputEl.value,
    });
  }
}

// 心跳函数 - 防止应用因超时而退出
async function sendHeartbeat() {
  try {
    await invoke("heartbeat");
    console.log("心跳发送成功");
  } catch (error) {
    console.error("心跳发送失败:", error);
  }
}

// 启动心跳定时器
function startHeartbeat() {
  // 立即发送第一次心跳
  sendHeartbeat();

  // 每5秒发送一次心跳（心跳超时时间是15秒）
  setInterval(sendHeartbeat, 5000);
}

window.addEventListener("DOMContentLoaded", () => {
  greetInputEl = document.querySelector("#greet-input");
  greetMsgEl = document.querySelector("#greet-msg");
  document.querySelector("#greet-form")?.addEventListener("submit", (e) => {
    e.preventDefault();
    greet();
  });

  // 启动心跳
  startHeartbeat();
});
