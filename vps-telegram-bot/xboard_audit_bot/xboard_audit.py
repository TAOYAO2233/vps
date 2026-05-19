#!/usr/bin/env python3
# -*- coding: utf-8 -*-

import re
import subprocess
import asyncio
import logging
from telegram import Bot
from telegram.constants import ParseMode

# ==================== 🛠️ 核心集中配置区域 ====================
# 1. 请在此处填写你的 Telegram 机器人 Token
TG_BOT_TOKEN = "**************************"

# 2. 在这里添加接收者的 Chat ID（可包含个人ID、群组ID、频道ID）
TG_CHAT_IDS = [
    6053576171,          
    5525443144,          
]

# 3. 聚合通知窗口（单位：秒）
PUSH_INTERVAL = 10  

# 4. 定义你需要监控的【日志文件路径】与【节点展示名称】的映射关系
# 💡 路径已全部适配到 /home/xboard_log/
LOG_TASKS = [
    {"path": "/home/xboard_log/xboard.log", "node_name": "vps-main-A"},
    {"path": "/home/xboard_log/xboard_vps_b.log", "node_name": "vps-node-B"}
]
# ========================================================

logging.basicConfig(level=logging.INFO, format='%(asctime)s [%(levelname)s] %(message)s')
connection_cache = []
bot = Bot(token=TG_BOT_TOKEN)

# 动态内存事务表：每个节点拥有独立的字典，防止多节点的 tx_id 互相覆盖或串流
tx_maps = {task["node_name"]: {} for task in LOG_TASKS}

# 核心正则 1：匹配入站来源 IP 行，提取：事务 ID、客户端 IP
# 兼容 rsyslog 传输可能附加的系统前缀，使用 search 捕获核心固定结构
FROM_REGEX = re.compile(
    r'INFO\s+\[(?P<tx_id>\d+)\s+\d+ms\]\s+inbound/\w+\[.*?\]:\s+inbound connection from\s+(?P<src_ip>[\d\.]+):\d+'
)

# 核心正则 2：匹配入站目标域名行，提取：时间、事务 ID、延迟、协议、UUID、目标网站
TO_REGEX = re.compile(
    r'(?P<time>\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})\s+INFO\s+\[(?P<tx_id>\d+)\s+(?P<delay>\d+)ms\]\s+'
    r'inbound/(?P<protocol>\w+)\[.*?\]:\s+\[(?P<uuid>[a-f0-9\-]+)\]\s+inbound connection to\s+(?P<target>.+)'
)

async def safe_tg_send(chat_id, text: str):
    """单目标安全发送函数：带超长自动二分切片逻辑，防止洪峰引发的报文超长拒绝投递"""
    try:
        await bot.send_message(chat_id=chat_id, text=text, parse_mode=ParseMode.MARKDOWN)
    except Exception as e:
        logging.error(f"目标 [{chat_id}] 发送失败: {e}")
        if "too long" in str(e).lower() or "message_too_long" in str(e):
            lines = text.split("\n")
            mid = len(lines) // 2
            header = lines[0] + "\n" + lines[1] + "\n" + lines[2] + "\n•───────────────────•\n"
            await safe_tg_send(chat_id, header + "\n".join(lines[4:mid]))
            await safe_tg_send(chat_id, header + "\n".join(lines[mid:]))

async def broadcast_message(text: str):
    """多用户并发广播"""
    tasks = [asyncio.create_task(safe_tg_send(cid, text)) for cid in TG_CHAT_IDS]
    await asyncio.gather(*tasks)

async def timer_aggregator_worker():
    """10秒时钟聚合器：将多路日志汇聚到一起后，在时间窗口内按节点分别打包发送"""
    global connection_cache
    while True:
        await asyncio.sleep(PUSH_INTERVAL)
        if connection_cache:
            batch_data = connection_cache.copy()
            connection_cache.clear()
            
            # 按照节点名称对数据进行分流分组
            nodes_data = {}
            for item in batch_data:
                nodes_data.setdefault(item['node_name'], []).append(item)
            
            # 分组渲染精美的高级排版报表
            for node_name, items in nodes_data.items():
                msg_header = (
                    f"📊 *节点网络访问审计 (集中管理版)*\n"
                    f"• 监控节点: `{node_name}`\n"
                    f"• 本周期总请求: `{len(items)}` 条\n"
                    f"•───────────────────•\n"
                )
                
                body_lines = []
                for item in items:
                    # 精简时间格式，只提取 时:分:秒
                    short_time = item['time'].split(' ')[1] if ' ' in item['time'] else item['time']
                    # 精简 UUID，只保留前 8 位核心特征
                    short_uuid = item['uuid'].split('-')[0] if '-' in item['uuid'] else item['uuid'][:8]
                    
                    line = (
                        f"🕒 `{short_time}` | `{item['protocol'].upper()}` | 👤 `{short_uuid}`\n"
                        f"🔌 来源: `{item['src_ip']}`\n"
                        f"🌐 目标: `{item['target'].strip()}` (`{item['delay']}ms`)"
                    )
                    body_lines.append(line)
                
                # 提交异步广播任务
                asyncio.create_task(broadcast_message(msg_header + "\n\n".join(body_lines)))

async def clean_tx_map_worker():
    """后台防御性清理器：每隔30秒检测一次各节点的事务表，超过阈值自动清空防止由于偶发漏配导致的内存堆积"""
    while True:
        await asyncio.sleep(30)
        for node_name, tx_map in tx_maps.items():
            if len(tx_map) > 2000:
                logging.warning(f"[{node_name}] 事务缓存表触发阈值防御，执行自动清空。")
                tx_map.clear()

async def tail_log_reader(file_path: str, node_name: str):
    """可复用的单路日志异步监听器"""
    process = subprocess.Popen(
        ['tail', '-F', '-n', '0', file_path], 
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
    )
    logging.info(f"成功建立逻辑审计监听流：[{node_name}] -> {file_path}")
    
    # 绑定当前节点专属的事务映射字典
    tx_ip_map = tx_maps[node_name]

    while True:
        line = await asyncio.to_thread(process.stdout.readline)
        if not line:
            await asyncio.sleep(0.01)
            continue
        
        # 步骤 1：拦截 From IP 报文行
        match_from = FROM_REGEX.search(line)
        if match_from:
            fd = match_from.groupdict()
            tx_ip_map[fd['tx_id']] = fd['src_ip']
            continue
            
        # 步骤 2：拦截 To Target 报文行
        match_to = TO_REGEX.search(line)
        if match_to:
            td = match_to.groupdict()
            tx_id = td['tx_id']
            
            # 核心交叉配对联查
            src_ip = tx_ip_map.get(tx_id, "未知IP")
            
            # 组装全量审计字典，并打上节点标志
            audit_item = {
                'node_name': node_name,
                'time': td['time'],
                'protocol': td['protocol'],
                'uuid': td['uuid'],
                'target': td['target'],
                'delay': td['delay'],
                'src_ip': src_ip
            }
            connection_cache.append(audit_item)
            
            # 消费完成后，立即释放该事务 ID
            if tx_id in tx_ip_map:
                del tx_ip_map[tx_id]

async def main():
    # 根据配置中的任务列表，动态并发生成多个 tail 日志读取线程
    readers = [tail_log_reader(task["path"], task["node_name"]) for task in LOG_TASKS]
    
    # 全量并发驱动
    await asyncio.gather(
        *readers,
        timer_aggregator_worker(),
        clean_tx_map_worker()
    )

if __name__ == '__main__':
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        logging.info("集中网络审计进程已被手动安全终止。")