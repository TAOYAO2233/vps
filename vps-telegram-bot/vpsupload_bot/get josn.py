# 在本地电脑运行此脚本
import os
from google.auth.transport.requests import Request
from google.oauth2.credentials import Credentials
from google_auth_oauthlib.flow import InstalledAppFlow

# 建议将 SCOPES 定义为常量
SCOPES = ['https://www.googleapis.com/auth/youtube.upload']

def get_authenticated_service():
    creds = None
    # 1. 动态获取当前脚本所在路径，确保 token.json 生成在同级目录
    basedir = os.path.dirname(os.path.abspath(__file__))
    token_path = os.path.join(basedir, 'token.json')
    secret_path = os.path.join(basedir, 'client_secrets.json')

    # 2. 尝试加载已有的凭据
    if os.path.exists(token_path):
        creds = Credentials.from_authorized_user_file(token_path, SCOPES)
    
    # 3. 如果凭据不存在、无效或已过期
    if not creds or not creds.valid:
        if creds and creds.expired and creds.refresh_token:
            # 如果 token 过期但有 refresh_token，则静默刷新（无需弹出浏览器）
            print("正在刷新访问令牌...")
            creds.refresh(Request())
        else:
            # 只有在完全没有凭据或无法刷新时，才进行交互式登录
            print("未发现有效凭据，正在请求人工授权...")
            flow = InstalledAppFlow.from_client_secrets_file(secret_path, SCOPES)
            creds = flow.run_local_server(port=0)
        
        # 4. 无论是新授权还是刷新后的凭据，都保存到本地
        with open(token_path, 'w') as token:
            token.write(creds.to_json())
            print(f"凭据已保存至: {token_path}")

    return creds

if __name__ == '__main__':
    credentials = get_authenticated_service()
    print("授权成功！您可以开始调用 YouTube API 了。")