// Experimental Q8 bridge. The service chooses original public model bytes;
// private activation snapshots stay in guest-local messages until its existing
// Shield backend masks them. Approximate arithmetic requires explicit opt-in.
std::string q8_base64(const unsigned char *data, size_t size) {
  std::string output(4*((size+2)/3)+1,'\0');
  int count=EVP_EncodeBlock(reinterpret_cast<unsigned char *>(output.data()),data,int(size));
  if(count<0) throw std::runtime_error("Q8 encoding");
  output.resize(size_t(count)); return output;
}
std::vector<unsigned char> q8_unbase64(const std::string &text, size_t expected) {
  if(text.size()!=4*((expected+2)/3)) throw std::runtime_error("Q8 result length");
  std::vector<unsigned char> output(text.size()/4*3);
  int count=EVP_DecodeBlock(output.data(),reinterpret_cast<const unsigned char *>(text.data()),int(text.size()));
  if(count<0 || size_t(count)<expected) throw std::runtime_error("Q8 result encoding");
  output.resize(expected);
  if(q8_base64(output.data(),output.size())!=text) throw std::runtime_error("Q8 noncanonical result");
  for(size_t i=0;i<expected;i+=4) {
    float value; std::memcpy(&value,output.data()+i,4);
    if(!std::isfinite(value)) throw std::runtime_error("Q8 nonfinite result");
  }
  return output;
}
std::optional<std::vector<unsigned char>> q8_masked(
    const std::vector<unsigned char> &input, const json &weights,
    size_t k, size_t n, size_t m, json &metadata) {
  static_assert(std::endian::native==std::endian::little && sizeof(float)==4);
  if(!weights.at("approved").get<bool>()) return std::nullopt;
  constexpr size_t maximum=2u<<20;
  const size_t rows=std::min({size_t(32),maximum/(k*4),maximum/(n*4)});
  if(!rows) return std::nullopt; // Public geometry alone cannot fit the protocol.
  std::vector<unsigned char> output(m*n*4);
  uint64_t exchanges=0, macs=0;
  for(size_t first=0;first<m;first+=rows) {
    const size_t count=std::min(rows,m-first);
    auto result=service({{"op","q8_0_matmul"},{"engine",weights.at("engine")},
        {"tensor",weights.at("tensor")},{"sha256",weights.at("sha256")},
        {"k",k},{"n",n},{"m",count},{"arithmetic","shield-fixed-point-v1"},
        {"input",q8_base64(input.data()+first*k*4,count*k*4)}});
    if(!result.at("supported").get<bool>()) {
      // Only a pre-execution decline of the first chunk can select CPU.
      if(first || result!=json{{"ok",true},{"supported",false}})
        throw std::runtime_error("Q8 late decline");
      return std::nullopt;
    }
    if(result.at("arithmetic")!="shield-fixed-point-v1" ||
       result.at("policySha256")!=weights.at("policySha256") ||
       result.at("tensorSha256")!=weights.at("sha256"))
      throw std::runtime_error("Q8 response binding");
    const auto &stats=result.at("stats");
    if(!stats.at("verification_failures").is_number_unsigned() ||
       stats.at("verification_failures")!=0 || !stats.at("exchanges").is_number_unsigned() ||
       !stats.at("macs").is_number_unsigned()) throw std::runtime_error("Q8 accounting");
    auto chunk=q8_unbase64(result.at("output").get<std::string>(),count*n*4);
    std::memcpy(output.data()+first*n*4,chunk.data(),chunk.size());
    const auto e=stats.at("exchanges").get<uint64_t>(), a=stats.at("macs").get<uint64_t>();
    if(e>UINT64_MAX-exchanges || a>UINT64_MAX-macs) throw std::runtime_error("Q8 counter overflow");
    exchanges+=e; macs+=a;
  }
  metadata["phase"]="verified-service-result";
  metadata["execution"]=exchanges?"masked-shield-service":"shield-service-no-new-exchange";
  metadata["arithmetic"]="shield-fixed-point-v1";
  metadata["maskedDispatch"]=exchanges!=0;
  metadata["verifiedExchanges"]=exchanges;
  metadata["verifiedMacs"]=macs;
  return output;
}
